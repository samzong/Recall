use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::db::search::TimeRange;
use crate::db::store::Store;
use crate::skill_audit::{self, SkillAuditFilters, SkillAuditReport};
use crate::sync::run_dashboard_sync_job;
use crate::usage::{self, UsageFilters, UsageReport};

#[derive(Debug)]
pub(crate) struct UsageRequest {
    pub(crate) id: u64,
    pub(crate) sources: Option<Vec<String>>,
    pub(crate) time_range: TimeRange,
    pub(crate) sync: bool,
}

pub(crate) struct UsageResponse {
    pub(crate) id: u64,
    pub(crate) sources: Option<Vec<String>>,
    pub(crate) time_range: TimeRange,
    pub(crate) current_report: Result<UsageReport, String>,
    pub(crate) all_time_report: Result<UsageReport, String>,
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
    let (current_report, all_time_report) =
        build_usage_reports(&request, |filters| usage::build_usage_report(store, filters));
    let skill_filters =
        SkillAuditFilters { sources: request.sources.clone(), time_range: request.time_range };
    let skill_audit_report = skill_audit::build_skill_audit_report(store, &skill_filters)
        .map_err(|error| error.to_string());

    UsageResponse {
        id: request.id,
        sources: request.sources,
        time_range: request.time_range,
        current_report,
        all_time_report,
        skill_audit_report,
    }
}

fn build_usage_reports(
    request: &UsageRequest,
    mut build: impl FnMut(&UsageFilters) -> anyhow::Result<UsageReport>,
) -> (Result<UsageReport, String>, Result<UsageReport, String>) {
    let current_filters =
        UsageFilters { sources: request.sources.clone(), time_range: request.time_range };
    let current_report = build(&current_filters).map_err(|error| error.to_string());

    let all_time_report = if request.time_range == TimeRange::All {
        current_report.clone()
    } else {
        let all_time_filters =
            UsageFilters { sources: request.sources.clone(), time_range: TimeRange::All };
        build(&all_time_filters).map_err(|error| error.to_string())
    };

    (current_report, all_time_report)
}

fn failed_response(request: UsageRequest, error: String) -> UsageResponse {
    UsageResponse {
        id: request.id,
        sources: request.sources,
        time_range: request.time_range,
        current_report: Err(error.clone()),
        all_time_report: Err(error.clone()),
        skill_audit_report: Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    fn request(time_range: TimeRange) -> UsageRequest {
        UsageRequest { id: 1, sources: None, time_range, sync: false }
    }

    #[test]
    fn identical_filters_build_one_usage_report() {
        let mut builds = 0;
        let (current, all_time) = build_usage_reports(&request(TimeRange::All), |_| {
            builds += 1;
            Ok(usage::aggregate_usage_events(&[]))
        });

        assert_eq!(builds, 1);
        assert!(current.is_ok());
        assert!(all_time.is_ok());
    }

    #[test]
    fn different_filters_keep_current_and_all_time_ranges() {
        let mut ranges = Vec::new();
        let (current, all_time) = build_usage_reports(&request(TimeRange::Month), |filters| {
            ranges.push(filters.time_range);
            Ok(usage::aggregate_usage_events(&[]))
        });

        assert_eq!(ranges, vec![TimeRange::Month, TimeRange::All]);
        assert!(current.is_ok());
        assert!(all_time.is_ok());
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

    #[test]
    fn request_returns_usage_and_skill_reports() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();

        let response = run_request(&store, request(TimeRange::Month));

        assert!(response.current_report.is_ok());
        assert!(response.all_time_report.is_ok());
        assert!(response.skill_audit_report.is_ok());
    }

    #[test]
    fn failure_reaches_every_report() {
        let response = failed_response(request(TimeRange::All), "sync failed".to_string());

        assert_eq!(response.current_report.unwrap_err(), "sync failed");
        assert_eq!(response.all_time_report.unwrap_err(), "sync failed");
        assert_eq!(response.skill_audit_report.unwrap_err(), "sync failed");
    }
}
