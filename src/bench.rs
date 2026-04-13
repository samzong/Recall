use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::Deserialize;

use crate::db::search::{SearchEngine, SearchFilters, TimeRange};
use crate::db::store::Store;
use crate::embedding::EmbeddingProvider;
use crate::semantic::build_embedding_text;
use crate::types::SearchResult;
use crate::utils::f32_slice_to_bytes;

const EVAL_TOP_K_MAX: usize = 20;

pub fn run_semantic() -> Result<()> {
    println!("=== Recall Semantic Pipeline Benchmark ===\n");

    let store = Store::open()?;

    let pending_pick: Option<(String, String, i64)> = store
        .conn
        .query_row(
            "SELECT m.session_id, COALESCE(s.title, m.session_id) AS title, COUNT(*) AS cnt
             FROM messages m
             JOIN sessions s ON s.id = m.session_id
             LEFT JOIN message_vec mv ON mv.message_id = m.id
             WHERE m.role = 'user' AND LENGTH(m.content) > 2 AND mv.message_id IS NULL
             GROUP BY m.session_id
             ORDER BY cnt DESC
             LIMIT 1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?)),
        )
        .optional()?;

    let (session_id, title, has_pending) = match pending_pick {
        Some((id, t, _)) => (id, t, true),
        None => {
            println!("No pending sessions found. Falling back to largest indexed session.\n");
            let fb: (String, String, i64) = store.conn.query_row(
                "SELECT m.session_id, COALESCE(s.title, m.session_id) AS title, COUNT(*) AS cnt
                 FROM messages m
                 JOIN sessions s ON s.id = m.session_id
                 WHERE m.role = 'user' AND LENGTH(m.content) > 2
                 GROUP BY m.session_id
                 ORDER BY cnt DESC
                 LIMIT 1",
                [],
                |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
                },
            )?;
            (fb.0, fb.1, false)
        }
    };

    println!("Target session: {title}");
    println!("  session_id : {session_id}");
    println!("  has_pending: {has_pending}\n");

    println!("[1/5] Cold model load ...");
    let t0 = Instant::now();
    let provider = EmbeddingProvider::new(false)?;
    let load_ms = t0.elapsed().as_millis();
    println!("      {load_ms} ms  (device: {})\n", provider.device_name());

    println!("[2/5] pending_embeddable_messages query ...");
    let t0 = Instant::now();
    let pending = store.pending_embeddable_messages(&session_id)?;
    let query_us = t0.elapsed().as_micros();
    println!("      {query_us} us  ({} rows)\n", pending.len());

    let messages: Vec<(i64, String)> = if pending.is_empty() {
        println!("      (using embeddable_messages fallback for inference test)\n");
        store.embeddable_messages(&session_id)?
    } else {
        pending
    };

    if messages.is_empty() {
        println!("No messages available to bench, aborting.");
        return Ok(());
    }

    println!("[3/5] build_embedding_text ...");
    let t0 = Instant::now();
    let texts: Vec<String> =
        messages.iter().map(|(_, c)| build_embedding_text(&title, c)).collect();
    let build_us = t0.elapsed().as_micros();
    let avg_len: usize =
        texts.iter().map(|t| t.chars().count()).sum::<usize>() / texts.len().max(1);
    println!("      {build_us} us  ({} texts, avg {} chars)\n", texts.len(), avg_len);

    println!("[4/5] Inference wall clock vs batch size");
    println!("      n = {} texts", texts.len());
    if texts.len() < 32 {
        println!("      note: n < 32, sweep will not reveal batch-size effects\n");
    } else {
        println!();
    }
    println!("      {:<10}{:<14}{:<14}throughput", "batch", "total_ms", "ms/msg");
    println!("      {:<10}{:<14}{:<14}----------", "-----", "--------", "------");

    let mut last_embeddings: Option<Vec<Vec<f32>>> = None;
    for bs in [4usize, 8, 16, 32, 64, 128, 256] {
        let t0 = Instant::now();
        let embs = provider.embed_documents_with_batch(&texts, bs)?;
        let elapsed = t0.elapsed();
        let total_ms = elapsed.as_millis();
        let per_msg = elapsed.as_secs_f64() * 1000.0 / texts.len() as f64;
        let thr = texts.len() as f64 / elapsed.as_secs_f64();
        println!("      {:<10}{:<14}{:<14.2}{:.1} msg/s", bs, total_ms, per_msg, thr);
        last_embeddings = Some(embs);
    }
    println!();

    println!("[5/5] DB upsert cost  (rollback-only, no data written)");
    if !has_pending {
        println!("      skipped: target session is already fully embedded, A/B would collide\n");
    } else {
        let embeddings = last_embeddings.expect("sweep produced embeddings");
        let items: Vec<(i64, &[f32])> = messages
            .iter()
            .zip(embeddings.iter())
            .map(|((id, _), emb)| (*id, emb.as_slice()))
            .collect();

        let current_us = time_upsert_current(&store, &items)?;
        let plain_us = time_upsert_plain(&store, &items)?;

        println!("      {:<32}{:>12} us", "current (DELETE + INSERT)", current_us);
        println!("      {:<32}{:>12} us", "alt     (plain INSERT)", plain_us);
        let diff = current_us as i128 - plain_us as i128;
        println!("      {:<32}{:>12} us", "diff", diff);
        if current_us > 0 {
            let pct = (diff.max(0) as f64) * 100.0 / current_us as f64;
            println!("      savings vs current: {pct:.1}%");
        }
        println!();
    }

    println!("=== Summary ===");
    println!("  cold model load : {load_ms} ms  (one-time per process)");
    println!("  pending query   : {query_us} us");
    println!("  text build      : {build_us} us");
    println!("  DB upsert       : typically < 1% of inference (see above)");
    println!("  dominant cost   : inference wall clock — see sweep table\n");

    Ok(())
}

pub fn run_search(query: &str) -> Result<()> {
    println!("=== Recall Search Cold-Path Benchmark ===\n");
    println!("  query: {query}\n");

    let t_open = Instant::now();
    let store = Store::open()?;
    let open_ms = t_open.elapsed().as_millis();

    let t_load = Instant::now();
    let provider = EmbeddingProvider::new(false)?;
    let load_ms = t_load.elapsed().as_millis();

    let t_embed = Instant::now();
    let query_embedding = provider
        .embed_query(&[query])?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty query embedding"))?;
    let embed_ms = t_embed.elapsed().as_millis();

    let engine = SearchEngine::new(&store.conn);
    let filters = SearchFilters { sources: None, time_range: TimeRange::All, directory: None };

    let t_search = Instant::now();
    let results = engine.hybrid_search(query, Some(&query_embedding), &filters, 20)?;
    let search_ms = t_search.elapsed().as_millis();

    let total_ms = open_ms + load_ms + embed_ms + search_ms;

    println!("  {:<18}{:>10}  {:>7}", "step", "ms", "%");
    println!("  {:<18}{:>10}  {:>7}", "----", "--", "-");
    let row = |name: &str, ms: u128| {
        let pct = if total_ms > 0 { (ms as f64) * 100.0 / total_ms as f64 } else { 0.0 };
        println!("  {name:<18}{ms:>10}  {pct:>6.1}%");
    };
    row("store open", open_ms);
    row("model load", load_ms);
    row("query embed", embed_ms);
    row("hybrid_search", search_ms);
    println!("  {:<18}{:>10}", "total", total_ms);
    println!("\n  ({} results)\n", results.len());

    Ok(())
}

fn time_upsert_current(store: &Store, items: &[(i64, &[f32])]) -> Result<u128> {
    store.conn.execute_batch("BEGIN")?;
    let t0 = Instant::now();
    {
        let mut del = store.conn.prepare("DELETE FROM message_vec WHERE message_id = ?1")?;
        let mut ins = store
            .conn
            .prepare("INSERT INTO message_vec (message_id, embedding) VALUES (?1, ?2)")?;
        for &(message_id, embedding) in items {
            let blob = f32_slice_to_bytes(embedding);
            del.execute(rusqlite::params![message_id])?;
            ins.execute(rusqlite::params![message_id, blob])?;
        }
    }
    let us = t0.elapsed().as_micros();
    store.conn.execute_batch("ROLLBACK")?;
    Ok(us)
}

fn time_upsert_plain(store: &Store, items: &[(i64, &[f32])]) -> Result<u128> {
    store.conn.execute_batch("BEGIN")?;
    let t0 = Instant::now();
    {
        let mut ins = store
            .conn
            .prepare("INSERT INTO message_vec (message_id, embedding) VALUES (?1, ?2)")?;
        for &(message_id, embedding) in items {
            let blob = f32_slice_to_bytes(embedding);
            ins.execute(rusqlite::params![message_id, blob])?;
        }
    }
    let us = t0.elapsed().as_micros();
    store.conn.execute_batch("ROLLBACK")?;
    Ok(us)
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExpectedSession {
    pub source: String,
    pub source_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EvalEntry {
    pub query: String,
    pub expected: Vec<ExpectedSession>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EvalFailure {
    pub query: String,
    pub rank: Option<usize>,
    pub expected: Vec<ExpectedSession>,
}

#[derive(Debug, Clone)]
pub struct ResultSummary {
    pub rank: usize,
    pub source: String,
    pub source_id: String,
    pub title: String,
    pub match_label: &'static str,
    pub is_expected: bool,
}

#[derive(Debug, Clone)]
pub struct QueryDetail {
    pub query: String,
    pub total_expected: usize,
    pub hits_in_top_k: usize,
    pub best_rank: Option<usize>,
    pub top_results: Vec<ResultSummary>,
}

#[derive(Debug, Clone, Default)]
pub struct EvalReport {
    pub total: usize,
    pub hit_at_5: usize,
    pub hit_at_10: usize,
    pub mrr_sum: f64,
    pub failures: Vec<EvalFailure>,
    pub details: Vec<QueryDetail>,
}

impl EvalReport {
    pub fn mrr(&self) -> f64 {
        if self.total == 0 { 0.0 } else { self.mrr_sum / self.total as f64 }
    }

    pub fn hit_at_5_pct(&self) -> f64 {
        pct(self.hit_at_5, self.total)
    }

    pub fn hit_at_10_pct(&self) -> f64 {
        pct(self.hit_at_10, self.total)
    }
}

fn pct(num: usize, denom: usize) -> f64 {
    if denom == 0 { 0.0 } else { (num as f64) * 100.0 / denom as f64 }
}

pub fn evaluate<F>(
    engine: &SearchEngine,
    entries: &[EvalEntry],
    embedder: F,
    top_k_max: usize,
) -> Result<EvalReport>
where
    F: Fn(&str) -> Option<Vec<f32>>,
{
    let filters = SearchFilters { sources: None, time_range: TimeRange::All, directory: None };
    let mut report = EvalReport { total: entries.len(), ..Default::default() };

    for entry in entries {
        let emb = embedder(&entry.query);
        let results = engine.hybrid_search(&entry.query, emb.as_deref(), &filters, top_k_max)?;
        let best_rank = find_best_rank(&results, &entry.expected);
        let hits_in_top_k = count_expected_hits(&results, &entry.expected);

        report.details.push(QueryDetail {
            query: entry.query.clone(),
            total_expected: entry.expected.len(),
            hits_in_top_k,
            best_rank,
            top_results: build_result_summaries(&results, &entry.expected, 5),
        });

        match best_rank {
            Some(rank) => {
                if rank <= 5 {
                    report.hit_at_5 += 1;
                }
                if rank <= 10 {
                    report.hit_at_10 += 1;
                }
                report.mrr_sum += 1.0 / rank as f64;
                if rank > 5 {
                    report.failures.push(EvalFailure {
                        query: entry.query.clone(),
                        rank: Some(rank),
                        expected: entry.expected.clone(),
                    });
                }
            }
            None => {
                report.failures.push(EvalFailure {
                    query: entry.query.clone(),
                    rank: None,
                    expected: entry.expected.clone(),
                });
            }
        }
    }

    Ok(report)
}

fn find_best_rank(results: &[SearchResult], expected: &[ExpectedSession]) -> Option<usize> {
    for (i, result) in results.iter().enumerate() {
        if is_expected(result, expected) {
            return Some(i + 1);
        }
    }
    None
}

fn count_expected_hits(results: &[SearchResult], expected: &[ExpectedSession]) -> usize {
    results.iter().filter(|r| is_expected(r, expected)).count()
}

fn is_expected(result: &SearchResult, expected: &[ExpectedSession]) -> bool {
    expected
        .iter()
        .any(|e| e.source == result.session.source && e.source_id == result.session.source_id)
}

fn build_result_summaries(
    results: &[SearchResult],
    expected: &[ExpectedSession],
    limit: usize,
) -> Vec<ResultSummary> {
    results
        .iter()
        .take(limit)
        .enumerate()
        .map(|(i, r)| ResultSummary {
            rank: i + 1,
            source: r.session.source.clone(),
            source_id: r.session.source_id.clone(),
            title: r.session.title.clone(),
            match_label: match r.match_source {
                crate::types::MatchSource::Fts => "FTS",
                crate::types::MatchSource::Vector => "VEC",
                crate::types::MatchSource::Hybrid => "HYB",
            },
            is_expected: is_expected(r, expected),
        })
        .collect()
}

pub fn run_eval(dataset_override: Option<&str>, verbose: bool) -> Result<()> {
    let dataset_path = resolve_dataset_path(dataset_override)?;
    let entries = load_dataset(&dataset_path)?;

    if entries.is_empty() {
        println!("No entries in dataset: {}", dataset_path.display());
        return Ok(());
    }

    let store = Store::open()?;
    let progress = store.semantic_progress().unwrap_or_default();
    let engine = SearchEngine::new(&store.conn);

    let provider_opt = if progress.done_sessions > 0 || progress.processing_sessions > 0 {
        match EmbeddingProvider::new(false) {
            Ok(p) => Some(p),
            Err(e) => {
                println!("Embedding unavailable: {e}");
                println!("Falling back to FTS-only evaluation.");
                println!();
                None
            }
        }
    } else {
        None
    };

    let mode = if provider_opt.is_some() { "hybrid (FTS + vector)" } else { "fts-only" };

    println!("Search Evaluation");
    println!("  dataset : {}", dataset_path.display());
    println!("  mode    : {mode}");
    println!("  entries : {}", entries.len());
    println!();

    let embedder = |q: &str| -> Option<Vec<f32>> {
        let provider = provider_opt.as_ref()?;
        provider.embed_query(&[q]).ok()?.into_iter().next()
    };

    let report = evaluate(&engine, &entries, embedder, EVAL_TOP_K_MAX)?;
    print_report(&report, EVAL_TOP_K_MAX, verbose);

    Ok(())
}

fn print_report(report: &EvalReport, top_k_max: usize, verbose: bool) {
    if verbose {
        print_per_query_details(report, top_k_max);
    }

    println!("Metrics");
    println!("  Hit@5   {}/{} ({:.1}%)", report.hit_at_5, report.total, report.hit_at_5_pct());
    println!("  Hit@10  {}/{} ({:.1}%)", report.hit_at_10, report.total, report.hit_at_10_pct());
    println!("  MRR     {:.3}", report.mrr());
    println!();

    if verbose || report.failures.is_empty() {
        return;
    }

    println!("Failures (rank > 5 or not in top-{top_k_max})");
    for failure in &report.failures {
        let rank_label = match failure.rank {
            Some(r) => format!("rank {r:<3}"),
            None => "miss    ".to_string(),
        };
        println!("  [{rank_label}] {}", failure.query);
        for expected in &failure.expected {
            println!("             expected {}/{}", expected.source, expected.source_id);
        }
    }
}

fn print_per_query_details(report: &EvalReport, top_k_max: usize) {
    for (idx, detail) in report.details.iter().enumerate() {
        let rank_label = match detail.best_rank {
            Some(r) => format!("rank {r}/{top_k_max}"),
            None => "MISS".to_string(),
        };
        println!("[{}/{}] {}", idx + 1, report.total, detail.query);
        println!(
            "  {} | {}/{} expected in top-{}",
            rank_label, detail.hits_in_top_k, detail.total_expected, top_k_max
        );
        println!("  top-{}:", detail.top_results.len());
        for r in &detail.top_results {
            let marker = if r.is_expected { '*' } else { ' ' };
            let title_short = shorten_title(&r.title, 70);
            println!(
                "    {rank:>2}. {marker} {lbl} {src}/{sid}",
                rank = r.rank,
                marker = marker,
                lbl = r.match_label,
                src = r.source,
                sid = r.source_id,
            );
            println!("         {title_short}");
        }
        println!();
    }
}

fn shorten_title(title: &str, max_chars: usize) -> String {
    let flat: String =
        title.chars().map(|c| if c.is_whitespace() || c.is_control() { ' ' } else { c }).collect();
    let normalized: String = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_chars {
        normalized
    } else {
        let head: String = normalized.chars().take(max_chars.saturating_sub(3)).collect();
        format!("{head}...")
    }
}

fn resolve_dataset_path(override_path: Option<&str>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        return Ok(PathBuf::from(p));
    }
    let data_dir = dirs::data_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine data directory"))?
        .join("recall");
    Ok(data_dir.join("search-eval.json"))
}

fn load_dataset(path: &Path) -> Result<Vec<EvalEntry>> {
    if !path.exists() {
        anyhow::bail!(
            "dataset not found: {}\nCreate one from search-eval.example.json or pass --dataset <PATH>.",
            path.display()
        );
    }
    let raw = std::fs::read_to_string(path)?;
    let entries: Vec<EvalEntry> = serde_json::from_str(&raw)?;
    Ok(entries)
}

pub fn dump_sessions() -> Result<()> {
    let store = Store::open()?;
    let mut stmt = store.conn.prepare(
        "SELECT source, source_id, started_at, title FROM sessions ORDER BY started_at DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;

    for row in rows {
        let (source, source_id, started_at, title) = row?;
        let ts = chrono::DateTime::from_timestamp_millis(started_at)
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
            .unwrap_or_else(|| "-".to_string());
        let flat_title = flatten_for_tsv(&title);
        println!("{source}\t{source_id}\t{ts}\t{flat_title}");
    }

    Ok(())
}

fn flatten_for_tsv(s: &str) -> String {
    let replaced: String =
        s.chars().map(|c| if c.is_whitespace() || c.is_control() { ' ' } else { c }).collect();
    replaced.split_whitespace().collect::<Vec<_>>().join(" ")
}
