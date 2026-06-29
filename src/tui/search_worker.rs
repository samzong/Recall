use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::db::search::{SearchEngine, SearchFilters};
use crate::db::store::Store;
use crate::embedding::EmbeddingProvider;
use crate::types::{MatchSource, SearchResult};

const SEARCH_LIMIT: usize = 200;
const FETCH_MULTIPLIER: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPhase {
    Text,
    Hybrid,
}

pub struct SearchRequest {
    pub id: u64,
    pub query: String,
    pub filters: SearchFilters,
    pub semantic_ready: bool,
}

pub struct SearchResponse {
    pub id: u64,
    pub query: String,
    pub phase: SearchPhase,
    pub result: Result<Vec<SearchResult>, String>,
}

pub struct SearchWorker {
    request_tx: Sender<SearchRequest>,
    response_rx: Receiver<SearchResponse>,
}

impl SearchWorker {
    pub fn spawn() -> Self {
        let (request_tx, request_rx) = mpsc::channel();
        let (response_tx, response_rx) = mpsc::channel();

        thread::spawn(move || run_worker(request_rx, response_tx));

        Self { request_tx, response_rx }
    }

    pub fn search(&self, request: SearchRequest) -> bool {
        self.request_tx.send(request).is_ok()
    }

    pub fn try_recv(&self) -> Option<SearchResponse> {
        self.response_rx.try_recv().ok()
    }
}

fn run_worker(request_rx: Receiver<SearchRequest>, response_tx: Sender<SearchResponse>) {
    crate::db::schema::register_sqlite_vec();

    let store = match Store::open() {
        Ok(store) => store,
        Err(err) => {
            while let Ok(request) = request_rx.recv() {
                send_error(
                    &response_tx,
                    request,
                    SearchPhase::Text,
                    format!("Search error: {err}"),
                );
            }
            return;
        }
    };

    let engine = SearchEngine::new(&store.conn);
    let mut provider = None;
    let mut embedding_unavailable = false;

    while let Ok(mut request) = request_rx.recv() {
        while let Ok(next) = request_rx.try_recv() {
            request = next;
        }

        run_request(
            &store,
            &engine,
            &mut provider,
            &mut embedding_unavailable,
            request,
            &response_tx,
        );
    }
}

fn run_request(
    store: &Store,
    engine: &SearchEngine,
    provider: &mut Option<EmbeddingProvider>,
    embedding_unavailable: &mut bool,
    request: SearchRequest,
    response_tx: &Sender<SearchResponse>,
) {
    if request.query.trim().is_empty() {
        send_result(response_tx, &request, SearchPhase::Text, recent_sessions(store, &request));
        return;
    }

    let text_result = engine.hybrid_search(
        &request.query,
        None,
        &request.filters,
        SEARCH_LIMIT,
        FETCH_MULTIPLIER,
    );
    let text_ok = text_result.is_ok();
    send_result(response_tx, &request, SearchPhase::Text, text_result);

    if !text_ok || !request.semantic_ready || *embedding_unavailable {
        return;
    }

    if provider.is_none() {
        match EmbeddingProvider::new(false) {
            Ok(next_provider) => {
                *provider = Some(next_provider);
            }
            Err(err) => {
                *embedding_unavailable = true;
                send_error(
                    response_tx,
                    request,
                    SearchPhase::Hybrid,
                    format!("Semantic unavailable - using text search only: {err}"),
                );
                return;
            }
        }
    }

    let embedding = provider
        .as_ref()
        .and_then(|p| p.embed_query(&[request.query.as_str()]).ok())
        .and_then(|mut e| if e.is_empty() { None } else { Some(e.swap_remove(0)) });

    let Some(embedding) = embedding else {
        send_error(
            response_tx,
            request,
            SearchPhase::Hybrid,
            "Semantic unavailable - using text search only".to_string(),
        );
        return;
    };

    let hybrid_result = engine.hybrid_search(
        &request.query,
        Some(embedding.as_slice()),
        &request.filters,
        SEARCH_LIMIT,
        FETCH_MULTIPLIER,
    );
    send_result(response_tx, &request, SearchPhase::Hybrid, hybrid_result);
}

fn recent_sessions(store: &Store, request: &SearchRequest) -> anyhow::Result<Vec<SearchResult>> {
    let recent = store.list_recent_sessions_for_search_scope(
        request.filters.sources.as_deref(),
        request.filters.time_range,
        request.filters.directory.as_deref(),
        request.filters.repo.as_ref(),
        SEARCH_LIMIT,
    )?;
    Ok(recent
        .into_iter()
        .map(|session| SearchResult { session, match_source: MatchSource::Fts, snippet: None })
        .collect())
}

fn send_result(
    response_tx: &Sender<SearchResponse>,
    request: &SearchRequest,
    phase: SearchPhase,
    result: anyhow::Result<Vec<SearchResult>>,
) {
    let _ = response_tx.send(SearchResponse {
        id: request.id,
        query: request.query.clone(),
        phase,
        result: result.map_err(|err| format!("Search error: {err}")),
    });
}

fn send_error(
    response_tx: &Sender<SearchResponse>,
    request: SearchRequest,
    phase: SearchPhase,
    error: String,
) {
    let _ = response_tx.send(SearchResponse {
        id: request.id,
        query: request.query,
        phase,
        result: Err(error),
    });
}
