use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::project_scope::ProjectScope;
use crate::semantic;
use crate::sync::{SyncRunOptions, run_sync_job_inner};

pub(crate) struct SyncRequest {
    pub(crate) sources: Option<Vec<String>>,
    pub(crate) scope: ProjectScope,
}

pub(crate) struct SyncWorker {
    request_tx: Sender<SyncRequest>,
    response_rx: Receiver<Result<(), String>>,
}

impl SyncWorker {
    pub(crate) fn spawn() -> Self {
        let (request_tx, request_rx) = mpsc::channel();
        let (response_tx, response_rx) = mpsc::channel();
        thread::spawn(move || run_worker(request_rx, response_tx));
        Self { request_tx, response_rx }
    }

    pub(crate) fn refresh(&self, request: SyncRequest) -> bool {
        self.request_tx.send(request).is_ok()
    }

    pub(crate) fn try_recv(&self) -> Option<Result<(), String>> {
        self.response_rx.try_recv().ok()
    }
}

fn run_worker(request_rx: Receiver<SyncRequest>, response_tx: Sender<Result<(), String>>) {
    while let Ok(request) = request_rx.recv() {
        let result = run_sync_job_inner(SyncRunOptions {
            force: false,
            verbose: false,
            emit: false,
            usage_only: false,
            backfill_events: false,
            sources: request.sources,
            scope: request.scope,
        })
        .and_then(|_| semantic::ensure_background_worker(false))
        .map_err(|error| format!("{error:#}"));
        if response_tx.send(result).is_err() {
            return;
        }
    }
}
