use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};

use crate::db::store::Store;
use crate::db::usage_store::SessionUsage;

pub(crate) struct ViewingUsageRequest {
    pub(crate) id: u64,
    pub(crate) session_id: String,
}

pub(crate) struct ViewingUsageResponse {
    pub(crate) id: u64,
    pub(crate) session_id: String,
    pub(crate) usage: Result<SessionUsage, String>,
}

pub(crate) struct ViewingUsageWorker {
    request_tx: Sender<ViewingUsageRequest>,
    response_rx: Receiver<ViewingUsageResponse>,
}

impl ViewingUsageWorker {
    pub(crate) fn spawn(path: PathBuf) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<ViewingUsageRequest>();
        let (response_tx, response_rx) = mpsc::channel();
        std::thread::spawn(move || {
            crate::db::schema::register_sqlite_vec();
            let store = Store::open_read_only_at(&path);
            let mut cached_totals = None;
            while let Ok(mut request) = request_rx.recv() {
                while let Ok(next) = request_rx.try_recv() {
                    request = next;
                }
                let usage = match &store {
                    Ok(store) => tokens_for_request(store, &request.session_id, &mut cached_totals),
                    Err(error) => Err(error.to_string()),
                };
                if response_tx
                    .send(ViewingUsageResponse {
                        id: request.id,
                        session_id: request.session_id,
                        usage,
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        Self { request_tx, response_rx }
    }

    pub(crate) fn refresh(&self, request: ViewingUsageRequest) -> bool {
        self.request_tx.send(request).is_ok()
    }

    pub(crate) fn try_recv(&self) -> Result<ViewingUsageResponse, mpsc::TryRecvError> {
        self.response_rx.try_recv()
    }
}

fn tokens_for_request(
    store: &Store,
    session_id: &str,
    cache: &mut Option<(i64, std::collections::HashMap<String, SessionUsage>)>,
) -> Result<SessionUsage, String> {
    let version = store
        .conn
        .pragma_query_value(None, "data_version", |row| row.get::<_, i64>(0))
        .map_err(|error| error.to_string())?;
    if cache.as_ref().is_none_or(|(cached_version, _)| *cached_version != version) {
        let totals = store.usage_by_session().map_err(|error| error.to_string())?;
        *cache = Some((version, totals));
    }
    Ok(cache.as_ref().and_then(|(_, totals)| totals.get(session_id)).cloned().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn session_totals_refresh_after_another_connection_changes_usage() {
        crate::db::schema::register_sqlite_vec();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recall.db");
        let store = Store::open_at(&path).unwrap();
        let session = crate::types::Session {
            source: "claude-code".into(),
            ..crate::types::test_support::session("owner")
        };
        let mut event = crate::types::test_support::usage_event("assistant:one");
        event.input_tokens = 10;
        store.persist_session_with_usage(&session, &[], &[event.clone()], Some(1)).unwrap();
        let worker = ViewingUsageWorker::spawn(path);
        for (id, expected) in [(1, 10), (2, 10), (3, 20)] {
            if id == 3 {
                event.input_tokens = 20;
                store.persist_session_with_usage(&session, &[], &[event.clone()], Some(1)).unwrap();
            }
            assert!(worker.refresh(ViewingUsageRequest { id, session_id: session.id.clone() }));
            let response = worker.response_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(response.id, id);
            assert_eq!(response.session_id, session.id);
            assert_eq!(response.usage.unwrap().tokens.total_tokens, expected);
        }
    }
}
