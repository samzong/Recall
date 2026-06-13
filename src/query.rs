use anyhow::Result;

use crate::adapters;
use crate::db::search::{SearchEngine, SearchFilters, TimeRange};
use crate::db::store::Store;
use crate::embedding::EmbeddingProvider;
use crate::types;
use crate::utils;

pub fn run_search(
    query: &str,
    source_filter: Option<&str>,
    time_filter: Option<&str>,
    project_filter: Option<&str>,
) -> Result<()> {
    let store = Store::open()?;
    let engine = SearchEngine::new(&store.conn);
    let sources = adapters::source_labels();
    let resolved_source = resolve_source_filter(source_filter, &sources)?;
    let time_range = parse_time_range(time_filter);
    let embedding = query_embedding(&store, query, |message| println!("{message}"))?;

    let filters = SearchFilters {
        sources: resolved_source,
        time_range,
        directory: project_filter.map(String::from),
    };

    let results = engine.hybrid_search(query, embedding.as_deref(), &filters, 20, 3)?;

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

pub fn resolve_source_filter(
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

pub fn parse_time_range(time_filter: Option<&str>) -> TimeRange {
    match time_filter.map(|t| t.to_lowercase()) {
        Some(ref t) if t == "today" => TimeRange::Today,
        Some(ref t) if t == "7d" || t == "week" => TimeRange::Week,
        Some(ref t) if t == "30d" || t == "month" => TimeRange::Month,
        _ => TimeRange::All,
    }
}

pub fn query_embedding<F>(store: &Store, query: &str, mut emit: F) -> Result<Option<Vec<f32>>>
where
    F: FnMut(&str),
{
    let progress = store.semantic_progress().unwrap_or_default();
    if progress.done_sessions == 0 && progress.processing_sessions == 0 {
        return Ok(None);
    }
    emit("Loading embedding model...");
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
            emit(&format!("Semantic unavailable: {e}"));
            Ok(None)
        }
    }
}
