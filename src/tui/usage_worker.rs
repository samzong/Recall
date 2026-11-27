use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::db::search::TimeRange;
use crate::db::store::Store;
use crate::skill_audit::{self, SkillAuditFilters, SkillAuditReport};
use crate::sync::run_dashboard_sync_job;
use crate::usage::{self, ProjectUsageReport, UsageReport, UsageTarget};

#[derive(Debug)]
pub(crate) struct UsageRequest {
    pub(crate) id: u64,
    pub(crate) sources: Option<Vec<String>>,
    pub(crate) time_range: TimeRange,
    pub(crate) target: UsageTarget,
    pub(crate) sync: bool,
}

pub(crate) struct UsageResponse {
    pub(crate) id: u64,
    pub(crate) sources: Option<Vec<String>>,
    pub(crate) time_range: TimeRange,
    pub(crate) target: UsageTarget,
    pub(crate) current_report: Result<UsageReport, String>,
    pub(crate) all_time_report: Result<UsageReport, String>,
    pub(crate) projects_report: Result<ProjectUsageReport, String>,
    pub(crate) skill_audit_report: Result<SkillAuditReport, String>,
}

pub(crate) struct UsageWorker {
    request_tx: Sender<UsageRequest>,
    response_rx: Receiver<UsageResponse>,
}

impl UsageWorker {
    pub(crate) fn spawn() -> Self {
        Self::spawn_with(run_worker)
    }

    fn spawn_with(
        run: impl FnOnce(Receiver<UsageRequest>, Sender<UsageResponse>) + Send + 'static,
    ) -> Self {
        let (request_tx, request_rx) = mpsc::channel();
        let (response_tx, response_rx) = mpsc::channel();
        thread::spawn(move || run(request_rx, response_tx));
        Self { request_tx, response_rx }
    }

    pub(crate) fn refresh(&self, request: UsageRequest) -> bool {
        self.request_tx.send(request).is_ok()
    }

    pub(crate) fn try_recv(&self) -> Option<UsageResponse> {
        self.response_rx.try_recv().ok()
    }
}

fn run_worker(request_rx: Receiver<UsageRequest>, response_tx: Sender<UsageResponse>) {
    crate::db::schema::register_sqlite_vec();

    let store = match Store::open() {
        Ok(store) => store,
        Err(error) => {
            while let Ok(request) = request_rx.recv() {
                let _ = response_tx
                    .send(failed_response(request, format!("Database unavailable: {error}")));
            }
            return;
        }
    };

    let mut sync_pending = false;
    while let Ok(mut request) = request_rx.recv() {
        while let Ok(next) = request_rx.try_recv() {
            let sync = request.sync || next.sync;
            request = UsageRequest { sync, ..next };
        }

        if let Err(error) = run_sync(&mut sync_pending, request.sync, run_dashboard_sync_job) {
            if response_tx.send(failed_response(request, error)).is_err() {
                return;
            }
            continue;
        }

        let response = run_request(&store, request);
        if response_tx.send(response).is_err() {
            return;
        }
    }
}

fn run_sync(
    pending: &mut bool,
    requested: bool,
    sync: impl FnOnce() -> anyhow::Result<()>,
) -> Result<(), String> {
    *pending |= requested;
    if *pending {
        sync().map_err(|error| format!("Sync failed: {error}"))?;
        *pending = false;
    }
    Ok(())
}

fn run_request(store: &Store, request: UsageRequest) -> UsageResponse {
    let bundle = usage::build_dashboard_bundle(
        store,
        request.sources.as_deref(),
        request.time_range,
        &request.target,
    );
    let (current_report, all_time_report, projects_report) = match bundle {
        Ok(bundle) => {
            (Ok(bundle.current_report), Ok(bundle.all_time_report), Ok(bundle.projects_report))
        }
        Err(error) => {
            let message = error.to_string();
            (Err(message.clone()), Err(message.clone()), Err(message))
        }
    };
    let skill_filters = SkillAuditFilters {
        sources: request.sources.clone(),
        time_range: request.time_range,
        target: request.target.clone(),
    };
    let skill_audit_report = skill_audit::build_skill_audit_report(store, &skill_filters)
        .map_err(|error| error.to_string());

    UsageResponse {
        id: request.id,
        sources: request.sources,
        time_range: request.time_range,
        target: request.target,
        current_report,
        all_time_report,
        projects_report,
        skill_audit_report,
    }
}

fn failed_response(request: UsageRequest, error: String) -> UsageResponse {
    UsageResponse {
        id: request.id,
        sources: request.sources,
        time_range: request.time_range,
        target: request.target,
        current_report: Err(error.clone()),
        all_time_report: Err(error.clone()),
        projects_report: Err(error.clone()),
        skill_audit_report: Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use crate::project_scope::ProjectScope;

    use super::*;

    fn request(time_range: TimeRange) -> UsageRequest {
        UsageRequest {
            id: 1,
            sources: None,
            time_range,
            target: UsageTarget::Scope(ProjectScope::Global),
            sync: false,
        }
    }

    #[test]
    fn request_returns_while_worker_is_busy() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = UsageWorker::spawn_with(move |request_rx, _| {
            request_rx.recv().unwrap();
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });

        assert!(worker.refresh(request(TimeRange::All)));
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(worker.try_recv().is_none());
        release_tx.send(()).unwrap();
    }

    #[test]
    fn failed_initial_sync_is_retried_by_the_next_request() {
        let mut pending = false;

        assert!(run_sync(&mut pending, true, || anyhow::bail!("offline")).is_err());
        assert!(pending);
        assert!(run_sync(&mut pending, false, || Ok(())).is_ok());
        assert!(!pending);
    }
}
