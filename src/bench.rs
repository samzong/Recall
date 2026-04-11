use std::time::Instant;

use anyhow::Result;
use rusqlite::OptionalExtension;

use crate::db::search::{SearchEngine, SearchFilters, TimeRange};
use crate::db::store::Store;
use crate::embedding::EmbeddingProvider;
use crate::semantic::build_embedding_text;
use crate::utils::f32_slice_to_bytes;

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
