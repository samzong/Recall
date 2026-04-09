use std::thread;
use std::time::Duration;

use anyhow::Result;

use crate::db::store::Store;
use crate::embedding::EmbeddingProvider;

const WORKER_IDLE_MS: u64 = 1500;
const SESSION_EMBED_BATCH: usize = 64;

pub struct SemanticRunOutcome {
    pub title: String,
    pub units_done: u64,
    pub units_total: u64,
}

pub fn start_background_worker() {
    if !EmbeddingProvider::is_available() {
        return;
    }

    thread::spawn(|| {
        let mut provider: Option<EmbeddingProvider> = None;

        loop {
            let store = match Store::open() {
                Ok(store) => store,
                Err(_) => {
                    thread::sleep(Duration::from_millis(WORKER_IDLE_MS));
                    continue;
                }
            };

            match store.has_pending_session_embeddings() {
                Ok(true) => {}
                Ok(false) | Err(_) => {
                    thread::sleep(Duration::from_millis(WORKER_IDLE_MS));
                    continue;
                }
            }

            if provider.is_none() {
                match EmbeddingProvider::new(false) {
                    Ok(p) => provider = Some(p),
                    Err(_) => {
                        thread::sleep(Duration::from_millis(WORKER_IDLE_MS));
                        continue;
                    }
                }
            }

            let outcome = match provider.as_ref() {
                Some(provider) => process_next_session(&store, provider),
                None => Ok(None),
            };

            match outcome {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => thread::sleep(Duration::from_millis(WORKER_IDLE_MS)),
            }
        }
    });
}

pub fn process_pending_sessions(limit: Option<usize>, show_progress: bool) -> Result<usize> {
    let store = Store::open()?;
    let provider = EmbeddingProvider::new(show_progress)?;
    let mut processed = 0usize;

    loop {
        if let Some(limit) = limit
            && processed >= limit
        {
            break;
        }

        let outcome = process_next_session(&store, &provider)?;
        let Some(outcome) = outcome else {
            break;
        };

        processed += 1;

        if show_progress {
            println!(
                "Semantic indexed: {} ({}/{})",
                outcome.title, outcome.units_done, outcome.units_total
            );
        }
    }

    Ok(processed)
}

fn process_next_session(
    store: &Store,
    provider: &EmbeddingProvider,
) -> Result<Option<SemanticRunOutcome>> {
    let Some(job) = store.claim_next_session_embedding_job()? else {
        return Ok(None);
    };

    let result = process_session(store, provider, &job);
    match result {
        Ok(outcome) => {
            store.complete_session_embedding(&job.session_id)?;
            Ok(Some(outcome))
        }
        Err(err) => {
            let message = format!("{err:#}");
            store.fail_session_embedding(&job.session_id, &message)?;
            Err(err)
        }
    }
}

fn process_session(
    store: &Store,
    provider: &EmbeddingProvider,
    job: &crate::types::SemanticSessionJob,
) -> Result<SemanticRunOutcome> {
    let pending = store.pending_embeddable_messages(&job.session_id)?;
    if pending.is_empty() {
        let units_done = store.embedded_message_count(&job.session_id)?;
        return Ok(SemanticRunOutcome {
            title: job.title.clone(),
            units_done,
            units_total: job.units_total,
        });
    }

    let mut units_done = store.embedded_message_count(&job.session_id)?;

    for chunk in pending.chunks(SESSION_EMBED_BATCH) {
        let texts: Vec<String> =
            chunk.iter().map(|(_, content)| build_embedding_text(&job.title, content)).collect();
        let embeddings = provider.embed_documents(&texts)?;
        let items: Vec<(i64, &[f32])> = chunk
            .iter()
            .zip(embeddings.iter())
            .map(|((message_id, _), embedding)| (*message_id, embedding.as_slice()))
            .collect();
        store.upsert_embeddings(&items)?;
        units_done += chunk.len() as u64;
        store.update_session_embedding_progress(&job.session_id, units_done)?;
    }

    Ok(SemanticRunOutcome { title: job.title.clone(), units_done, units_total: job.units_total })
}

fn build_embedding_text(title: &str, content: &str) -> String {
    let text = format!("{title}: {content}");
    if text.chars().count() > 500 { text.chars().take(500).collect() } else { text }
}
