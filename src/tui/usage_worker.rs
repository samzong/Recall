use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::db::search::TimeRange;
use crate::db::store::Store;
use crate::skill_audit::{self, SkillAuditFilters, SkillAuditReport};
use crate::usage::{self, ProjectUsageReport, UsageReport, UsageTarget};

#[derive(Debug)]
pub(crate) struct UsageRequest {
    pub(crate) id: u64,
    pub(crate) sources: Option<Vec<String>>,
    pub(crate) time_range: TimeRange,
    pub(crate) target: UsageTarget,
}

pub(crate) struct DashboardReports {
    pub(crate) current_report: Result<UsageReport, String>,
    pub(crate) all_time_report: Result<UsageReport, String>,
    pub(crate) projects_report: Result<ProjectUsageReport, String>,
}

pub(crate) enum UsagePayload {
    Dashboard(Box<DashboardReports>),
    Skills(Result<SkillAuditReport, String>),
}

pub(crate) struct UsageResponse {
    pub(crate) id: u64,
    pub(crate) sources: Option<Vec<String>>,
    pub(crate) time_range: TimeRange,
    pub(crate) target: UsageTarget,
    pub(crate) payload: UsagePayload,
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

    let path = match Store::default_db_path() {
        Ok(path) => path,
        Err(error) => {
            send_unavailable(request_rx, response_tx, error.to_string());
            return;
        }
    };
    let store = match Store::open_read_only_at(&path) {
        Ok(store) => store,
        Err(error) => {
            send_unavailable(request_rx, response_tx, format!("Database unavailable: {error}"));
            return;
        }
    };

    run_worker_with_store(&store, request_rx, response_tx);
}

fn run_worker_with_store(
    store: &Store,
    request_rx: Receiver<UsageRequest>,
    response_tx: Sender<UsageResponse>,
) {
    let mut next_request = None;
    loop {
        let mut request = match next_request.take() {
            Some(request) => request,
            None => match request_rx.recv() {
                Ok(request) => request,
                Err(_) => return,
            },
        };
        while let Ok(next) = request_rx.try_recv() {
            request = next;
        }

        let dashboard = build_dashboard_response(store, &request);
        if response_tx.send(dashboard).is_err() {
            return;
        }

        if let Ok(next) = request_rx.try_recv() {
            next_request = Some(next);
            continue;
        }

        let skills = build_skills_response(store, request);
        if response_tx.send(skills).is_err() {
            return;
        }
    }
}

fn send_unavailable(
    request_rx: Receiver<UsageRequest>,
    response_tx: Sender<UsageResponse>,
    error: String,
) {
    while let Ok(request) = request_rx.recv() {
        let dashboard = UsageResponse {
            id: request.id,
            sources: request.sources.clone(),
            time_range: request.time_range,
            target: request.target.clone(),
            payload: UsagePayload::Dashboard(Box::new(DashboardReports {
                current_report: Err(error.clone()),
                all_time_report: Err(error.clone()),
                projects_report: Err(error.clone()),
            })),
        };
        if response_tx.send(dashboard).is_err() {
            return;
        }
        let skills = UsageResponse {
            id: request.id,
            sources: request.sources,
            time_range: request.time_range,
            target: request.target,
            payload: UsagePayload::Skills(Err(error.clone())),
        };
        if response_tx.send(skills).is_err() {
            return;
        }
    }
}

fn build_dashboard_response(store: &Store, request: &UsageRequest) -> UsageResponse {
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

    UsageResponse {
        id: request.id,
        sources: request.sources.clone(),
        time_range: request.time_range,
        target: request.target.clone(),
        payload: UsagePayload::Dashboard(Box::new(DashboardReports {
            current_report,
            all_time_report,
            projects_report,
        })),
    }
}

fn build_skills_response(store: &Store, request: UsageRequest) -> UsageResponse {
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
        payload: UsagePayload::Skills(skill_audit_report),
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
    fn worker_emits_dashboard_before_skills_for_request() {
        let worker = UsageWorker::spawn_with(|request_rx, response_tx| {
            crate::db::schema::register_sqlite_vec();
            let store = Store::open_in_memory().unwrap();
            run_worker_with_store(&store, request_rx, response_tx);
        });

        assert!(worker.refresh(request(TimeRange::All)));
        let first = worker.response_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(first.payload, UsagePayload::Dashboard(_)));
        let second = worker.response_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(second.payload, UsagePayload::Skills(_)));
    }
}
