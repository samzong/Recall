use std::collections::HashSet;
use std::path::PathBuf;

use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::{
    AdapterSyncContext, InventoryIssue, RawMessage, RawSession, ReconcilePlan, ResumeCommand,
    SourceAdapter, SourceObservation, SyncScanOutput, SyncScanResult,
};
use crate::config::AppConfig;
use crate::db::{
    schema,
    search::RepoFilter,
    store::{SessionPath, Store},
};
use crate::project_scope::ProjectScope;
use crate::types::{Message, Role, Session};

use super::{
    BackfillPlan, ExistingSessionAction, SyncJob, SyncRunOptions, decide_existing_session_action,
    delete_excluded_sessions_for_source, raw_session_metadata_changed,
};

struct StaticAdapter {
    updated_at: i64,
    messages: &'static [&'static str],
    source_file_path: Option<&'static str>,
    optimized: bool,
}

struct FailingAdapter;

struct ReconcileAdapter {
    plan: ReconcilePlan,
    include_session: bool,
}

struct ObservationAdapter {
    directory: Option<&'static str>,
    include_session: bool,
    failing_path: Option<PathBuf>,
}

impl SourceAdapter for FailingAdapter {
    fn id(&self) -> &str {
        "test"
    }

    fn label(&self) -> &str {
        "Test"
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        anyhow::bail!("injected scan failure")
    }

    fn resume_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        None
    }
}

impl SourceAdapter for ReconcileAdapter {
    fn id(&self) -> &str {
        "test"
    }

    fn label(&self) -> &str {
        "Test"
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        if self.include_session {
            return Ok(vec![RawSession::search_only(
                "new",
                None,
                1_000,
                Some(2_000),
                None,
                vec![RawMessage {
                    role: Role::User,
                    content: "new".to_string(),
                    timestamp: Some(2_000),
                }],
            )]);
        }
        Ok(Vec::new())
    }

    fn scan_for_sync_output(
        &self,
        _context: &AdapterSyncContext,
        _since_ts: Option<i64>,
        _include_events: bool,
        _force: bool,
    ) -> anyhow::Result<Option<SyncScanOutput>> {
        Ok(Some(SyncScanOutput {
            scan: SyncScanResult {
                sessions: self.scan()?,
                stats: crate::adapters::SyncScanStats {
                    candidates: u32::from(self.include_session),
                    parsed: u32::from(self.include_session),
                    ..Default::default()
                },
                observations: Vec::new(),
            },
            reconcile: Some(self.plan.clone()),
        }))
    }

    fn resume_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        None
    }
}

impl SourceAdapter for ObservationAdapter {
    fn id(&self) -> &str {
        "test"
    }

    fn label(&self) -> &str {
        "Test"
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        if self.include_session {
            return Ok(vec![RawSession::search_only(
                "new",
                self.directory.map(str::to_string),
                1_000,
                Some(2_000),
                None,
                vec![RawMessage {
                    role: Role::User,
                    content: "new".to_string(),
                    timestamp: Some(2_000),
                }],
            )]);
        }
        Ok(Vec::new())
    }

    fn scan_for_sync_output(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        _include_events: bool,
        _force: bool,
    ) -> anyhow::Result<Option<SyncScanOutput>> {
        if let Some(path) = &self.failing_path {
            let entry = FileScanEntry {
                session_id: "observed".to_string(),
                stat_target: path.clone(),
                directory: None,
            };
            let scan = file_scan::run_file_scan_with_options(
                context,
                since_ts,
                Default::default(),
                vec![entry],
                |_, _| anyhow::bail!("injected parse failure"),
            )?;
            return Ok(Some(SyncScanOutput { scan, reconcile: None }));
        }
        Ok(Some(SyncScanOutput {
            scan: SyncScanResult {
                sessions: self.scan()?,
                stats: Default::default(),
                observations: vec![SourceObservation {
                    source_id: "observed".to_string(),
                    source_file_path: Some("/tmp/observed.jsonl".to_string()),
                }],
            },
            reconcile: None,
        }))
    }

    fn resume_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        None
    }
}

impl SourceAdapter for StaticAdapter {
    fn id(&self) -> &str {
        "test"
    }

    fn label(&self) -> &str {
        "Test"
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let messages = self
            .messages
            .iter()
            .enumerate()
            .map(|(seq, content)| RawMessage {
                role: Role::User,
                content: (*content).to_string(),
                timestamp: Some(self.updated_at + seq as i64),
            })
            .collect();
        let mut raw =
            RawSession::search_only("raw1", None, 1_000, Some(self.updated_at), None, messages);
        raw.source_file_path = self.source_file_path.map(str::to_string);
        Ok(vec![raw])
    }

    fn scan_for_sync(
        &self,
        _context: &AdapterSyncContext,
        _since_ts: Option<i64>,
        _include_events: bool,
    ) -> anyhow::Result<Option<crate::adapters::SyncScanResult>> {
        if !self.optimized {
            return Ok(None);
        }
        Ok(Some(crate::adapters::SyncScanResult {
            sessions: self.scan()?,
            stats: Default::default(),
            observations: Vec::new(),
        }))
    }

    fn resume_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        None
    }
}

fn matcher(pattern: &str) -> globset::GlobSet {
    let mut builder = globset::GlobSetBuilder::new();
    builder.add(globset::Glob::new(pattern).unwrap());
    builder.build().unwrap()
}

fn session(id: &str, source: &str, source_id: &str) -> Session {
    Session {
        source: source.to_string(),
        source_id: source_id.to_string(),
        title: "t".to_string(),
        updated_at: Some(1),
        ..crate::types::test_support::session(id)
    }
}

fn single_session_job(adapters: &[Box<dyn SourceAdapter>], target: &str) -> SyncJob {
    let mut job = global_job(adapters);
    job.config.sync_window = crate::config::SyncWindow::Today;
    SyncJob::new(
        SyncRunOptions { target_session: Some(target.to_string()), ..job.options },
        job.store,
        job.config,
        adapters,
    )
    .unwrap()
}

fn global_job(adapters: &[Box<dyn SourceAdapter>]) -> SyncJob {
    schema::register_sqlite_vec();
    SyncJob::new(
        SyncRunOptions {
            force: false,
            verbose: false,
            emit: false,
            usage_only: false,
            backfill_events: false,
            sources: None,
            scope: ProjectScope::Global,
            target_session: None,
        },
        Store::open_in_memory().unwrap(),
        AppConfig::default(),
        adapters,
    )
    .unwrap()
}

#[test]
fn usage_only_never_refreshes_existing_session() {
    assert_eq!(
        decide_existing_session_action(true, false, false, true, true, true, true),
        ExistingSessionAction::BackfillOnly(BackfillPlan {
            usage: true,
            events: false,
            metadata: false
        })
    );
    assert_eq!(
        decide_existing_session_action(true, false, false, true, false, true, true),
        ExistingSessionAction::Skip
    );
}

#[test]
fn usage_only_can_backfill_events_without_refresh() {
    assert_eq!(
        decide_existing_session_action(true, true, false, true, false, true, false),
        ExistingSessionAction::BackfillOnly(BackfillPlan {
            usage: false,
            events: true,
            metadata: false
        })
    );
    assert_eq!(
        decide_existing_session_action(true, true, false, true, true, true, false),
        ExistingSessionAction::BackfillOnly(BackfillPlan {
            usage: true,
            events: true,
            metadata: false
        })
    );
    for (old_texts, expected_anchor) in [
        (vec!["question", "tool payload", "answer"], None),
        (vec!["question", "different answer"], None),
        (vec!["question", "answer"], Some(1)),
    ] {
        for backfill_events in [false, true] {
            let mut job = global_job(&[]);
            job.options.usage_only = true;
            job.options.backfill_events = backfill_events;
            let mut old = session("anchor-test", "test", "raw1");
            old.message_count = old_texts.len() as u32;
            job.store.insert_session(&old).unwrap();
            job.store
                .insert_messages(
                    &old_texts
                        .iter()
                        .enumerate()
                        .map(|(index, text)| Message {
                            session_id: old.id.clone(),
                            role: if index == 0 { Role::User } else { Role::Assistant },
                            content: text.to_string(),
                            timestamp: Some(1),
                            seq: index as u32,
                        })
                        .collect::<Vec<_>>(),
                )
                .unwrap();
            let make_raw = || {
                let event = crate::adapters::events::tool_call_event(
                    crate::adapters::events::EventContext {
                        event_seq: 0,
                        timestamp: Some(1),
                        source_path: Some("native.jsonl".to_string()),
                        source_event_id: Some("native-record".to_string()),
                        message_seq: Some(1),
                        parser_version: 2,
                    },
                    "Edit".to_string(),
                    Some(&serde_json::json!({"file_path": "a.rs"})),
                );
                let usage = crate::types::RawUsageEvent {
                    message_seq: Some(1),
                    timestamp: 1,
                    model: "model".to_string(),
                    provider: "provider".to_string(),
                    input_tokens: 10,
                    output_tokens: 2,
                    token_source: crate::types::TokenSource::Observed,
                    parser_version: 2,
                    source_path: Some("native.jsonl".to_string()),
                    ..crate::types::test_support::usage_event("usage-1")
                };
                let mut raw = RawSession::search_only(
                    "raw1",
                    None,
                    0,
                    Some(1),
                    None,
                    vec![
                        RawMessage {
                            role: Role::User,
                            content: "question".to_string(),
                            timestamp: Some(1),
                        },
                        RawMessage {
                            role: Role::Assistant,
                            content: "answer".to_string(),
                            timestamp: Some(1),
                        },
                    ],
                )
                .with_usage(vec![usage], 2)
                .with_events(vec![event], 2);
                raw.metadata_parser_version = Some(1);
                raw.refresh_session_on_metadata_backfill = true;
                raw
            };
            let context = job.load_adapter_sync_context("test").unwrap();
            let mut existing = job.prepare_existing_state("test", context).unwrap();
            job.process_raw_session("test", make_raw(), &mut existing, &mut HashSet::new())
                .unwrap();
            assert_eq!(
                job.store
                    .get_messages(&old.id)
                    .unwrap()
                    .iter()
                    .map(|message| message.content.as_str())
                    .collect::<Vec<_>>(),
                old_texts
            );
            assert_eq!(
                job.store.list_usage_events_for_session(&old.id).unwrap()[0].message_seq,
                expected_anchor
            );
            let events = job.store.list_session_events_for_session(&old.id).unwrap();
            if backfill_events {
                assert_eq!(events[0].message_seq, expected_anchor);
                assert_eq!(events[0].source_event_id.as_deref(), Some("native-record"));
            } else {
                assert!(events.is_empty());
            }
            job.options.usage_only = false;
            job.process_raw_session("test", make_raw(), &mut existing, &mut HashSet::new())
                .unwrap();
            assert_eq!(
                job.store
                    .get_messages(&old.id)
                    .unwrap()
                    .iter()
                    .map(|message| message.content.as_str())
                    .collect::<Vec<_>>(),
                vec!["question", "answer"]
            );
            assert_eq!(
                job.store.list_session_events_for_session(&old.id).unwrap()[0].message_seq,
                Some(1)
            );
            assert_eq!(
                job.store.list_usage_events_for_session(&old.id).unwrap()[0].message_seq,
                Some(1)
            );
        }
    }
}

#[test]
fn event_maintenance_preserves_discussions_and_is_resumable() {
    struct HistoryAdapter;
    impl SourceAdapter for HistoryAdapter {
        fn id(&self) -> &str {
            "claude-code"
        }
        fn label(&self) -> &str {
            "Claude Code"
        }
        fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
            Ok(["existing", "new", "excluded"]
                .into_iter()
                .map(|id| {
                    let mut raw = RawSession::search_only(
                        id,
                        Some(format!("/work/{id}")),
                        0,
                        Some(1),
                        None,
                        vec![RawMessage {
                            role: Role::User,
                            content: "native discussion".to_string(),
                            timestamp: Some(1),
                        }],
                    );
                    raw.custom_title = Some("native title".to_string());
                    raw.thread_role = Some(crate::types::ThreadRole::Subagent);
                    raw.parent_links = vec![crate::types::ParentLink {
                        relation: crate::types::ParentRelation::Spawn,
                        source: "claude-code".into(),
                        source_id: "parent".into(),
                    }];
                    raw.metadata_parser_version = Some(1);
                    raw.source_file_path = Some(format!("/native/{id}"));
                    raw.with_events(
                        vec![crate::adapters::events::tool_call_event(
                            crate::adapters::events::EventContext {
                                event_seq: 0,
                                timestamp: Some(1),
                                source_path: Some(format!("/native/{id}")),
                                source_event_id: Some("record".to_string()),
                                message_seq: Some(0),
                                parser_version: 2,
                            },
                            "Edit".to_string(),
                            Some(&serde_json::json!({"file_path":"a.rs"})),
                        )],
                        2,
                    )
                })
                .collect())
        }
        fn scan_for_sync_output(
            &self,
            _: &AdapterSyncContext,
            since: Option<i64>,
            include_events: bool,
            _: bool,
        ) -> anyhow::Result<Option<SyncScanOutput>> {
            assert_eq!(since, None);
            assert!(include_events);
            let sessions = self.scan()?;
            Ok(Some(SyncScanOutput {
                reconcile: Some(ReconcilePlan::CompleteLiveSet(
                    sessions.iter().map(|raw| raw.source_id.clone()).collect(),
                )),
                scan: SyncScanResult {
                    sessions,
                    stats: crate::adapters::SyncScanStats { parsed: 3, ..Default::default() },
                    observations: Vec::new(),
                },
            }))
        }
        fn resume_command(&self, _: &str) -> Option<ResumeCommand> {
            None
        }
    }
    let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(HistoryAdapter)];
    let mut job = global_job(&adapters);
    job.options.usage_only = true;
    job.options.backfill_events = true;
    job.since_ts = None;
    job.path_excluder = Some(matcher("/work/excluded"));
    for id in ["existing", "missing", "excluded"] {
        let mut old = session(id, "claude-code", id);
        old.directory = Some(format!("/work/{id}"));
        old.source_file_path = Some(format!("/old/{id}"));
        old.is_import = true;
        old.message_count = 1;
        job.store.insert_session(&old).unwrap();
        job.store
            .insert_messages(&[Message {
                session_id: id.to_string(),
                role: Role::User,
                content: "stored discussion".to_string(),
                timestamp: Some(1),
                seq: 0,
            }])
            .unwrap();
    }
    let usage = crate::types::RawUsageEvent {
        message_seq: Some(0),
        timestamp: 1,
        model: "model".to_string(),
        provider: "provider".to_string(),
        input_tokens: 10,
        output_tokens: 2,
        token_source: crate::types::TokenSource::Observed,
        ..crate::types::test_support::usage_event("usage")
    };
    job.store
        .persist_usage_events_for_existing_session("claude-code", "existing", &[usage], 1, Some(1))
        .unwrap();
    job.event_backfill = Some(super::EventBackfillReport { dry_run: true, ..Default::default() });
    job.run_with(&adapters, None).unwrap();
    assert_eq!(
        (job.stats.new_sessions, job.stats.reprocessed_sessions, job.stats.excluded_out),
        (1, 1, 1)
    );
    assert_eq!(job.event_backfill.as_ref().unwrap().missing_original, 1);
    assert!(job.store.list_session_events_for_session("existing").unwrap().is_empty());
    assert!(!job.store.session_meta_map("claude-code").unwrap().contains_key("new"));
    job.stats = Default::default();
    job.event_backfill = Some(super::EventBackfillReport::default());
    job.run_with(&adapters, None).unwrap();
    assert_eq!(
        (job.stats.new_sessions, job.stats.reprocessed_sessions, job.stats.excluded_out),
        (1, 1, 1)
    );
    for id in ["existing", "missing", "excluded"] {
        let stored = job.store.get_session_by_id(id).unwrap().unwrap();
        assert!(stored.is_import);
        assert_eq!(stored.title, "t");
        assert_eq!(stored.source_file_path.as_deref(), Some(format!("/old/{id}").as_str()));
        assert_eq!(job.store.get_messages(id).unwrap()[0].content, "stored discussion");
    }
    let events = job.store.list_session_events_for_session("existing").unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].message_seq, None);
    assert_eq!(job.store.list_usage_events_for_session("existing").unwrap()[0].input_tokens, 10);
    let new_id = job.store.session_meta_map("claude-code").unwrap()["new"].id.clone();
    assert_eq!(job.store.get_messages(&new_id).unwrap()[0].content, "native discussion");
    assert_eq!(job.store.list_session_events_for_session(&new_id).unwrap()[0].message_seq, Some(0));
    assert!(job.store.list_usage_events_for_session(&new_id).unwrap().is_empty());
    let (embedding_id, status): (String, String) = job
        .store
        .conn
        .query_row("SELECT session_id, status FROM session_embedding_state", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(embedding_id, new_id);
    assert_eq!(status, "pending");
    let role: String = job
        .store
        .conn
        .query_row("SELECT thread_role FROM sessions WHERE id = ?1", [&new_id], |row| row.get(0))
        .unwrap();
    assert_eq!(role, "subagent");
    let links = job.store.session_topology(&new_id).unwrap();
    assert_eq!(links.parents.len(), 1);
    assert_eq!(links.parents[0].source_id, "parent");
    job.stats = Default::default();
    job.event_backfill = Some(super::EventBackfillReport::default());
    job.run_with(&adapters, None).unwrap();
    assert_eq!(
        (job.stats.new_sessions, job.stats.reprocessed_sessions, job.stats.skipped),
        (0, 0, 2)
    );
    assert_eq!(job.store.list_session_events_for_session("existing").unwrap().len(), 1);
}

#[test]
fn full_sync_refreshes_changed_existing_session() {
    assert_eq!(
        decide_existing_session_action(false, false, false, true, true, true, false),
        ExistingSessionAction::RefreshSession
    );
}

#[test]
fn full_sync_backfills_unchanged_existing_session_in_place() {
    assert_eq!(
        decide_existing_session_action(false, false, false, false, true, true, false),
        ExistingSessionAction::BackfillOnly(BackfillPlan {
            usage: true,
            events: true,
            metadata: false
        })
    );
    assert_eq!(
        decide_existing_session_action(false, false, false, false, false, false, false),
        ExistingSessionAction::Skip
    );
}

#[test]
fn full_sync_backfills_metadata_only_when_topology_parser_advances() {
    assert_eq!(
        decide_existing_session_action(false, false, false, false, false, false, true),
        ExistingSessionAction::BackfillOnly(BackfillPlan {
            usage: false,
            events: false,
            metadata: true
        })
    );
    assert_eq!(
        decide_existing_session_action(true, false, false, false, false, false, true),
        ExistingSessionAction::Skip
    );
}

#[test]
fn full_sync_treats_new_session_metadata_as_changed() {
    let raw = RawSession::search_only(
        "raw1",
        Some("/Users/x/git/samzong/Recall".to_string()),
        0,
        Some(1),
        None,
        vec![],
    );
    let missing = SessionPath {
        source_id: "raw1".to_string(),
        directory: None,
        source_file_path: None,
        repo_remote: None,
        repo_slug: None,
        repo_name: None,
    };
    let same = SessionPath {
        source_id: "raw1".to_string(),
        directory: Some("/Users/x/git/samzong/Recall".to_string()),
        source_file_path: None,
        repo_remote: Some("github.com/samzong/Recall".to_string()),
        repo_slug: None,
        repo_name: None,
    };
    assert!(raw_session_metadata_changed(&raw, None, &missing));
    assert!(!raw_session_metadata_changed(&raw, None, &same));

    let mut raw_with_path = RawSession::search_only("raw1", None, 0, Some(1), None, vec![]);
    raw_with_path.source_file_path = Some("/tmp/session.jsonl".to_string());
    assert!(raw_session_metadata_changed(&raw_with_path, None, &missing));
}

struct TwoProjectAdapter;

impl SourceAdapter for TwoProjectAdapter {
    fn id(&self) -> &str {
        "test"
    }

    fn label(&self) -> &str {
        "Test"
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let message = |content: &str| RawMessage {
            role: Role::User,
            content: content.to_string(),
            timestamp: Some(1_000),
        };
        Ok(vec![
            RawSession::search_only(
                "inside",
                Some("/repo/root/nested".to_string()),
                1_000,
                Some(2_000),
                None,
                vec![message("inside")],
            ),
            RawSession::search_only(
                "outside",
                Some("/elsewhere".to_string()),
                1_000,
                Some(2_000),
                None,
                vec![message("outside")],
            ),
        ])
    }

    fn resume_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        None
    }
}

fn scoped_job(scope: ProjectScope) -> (SyncJob, Vec<Box<dyn SourceAdapter>>) {
    schema::register_sqlite_vec();
    let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(TwoProjectAdapter)];
    let job = SyncJob::new(
        SyncRunOptions {
            force: false,
            verbose: false,
            emit: false,
            usage_only: false,
            backfill_events: false,
            sources: None,
            scope,
            target_session: None,
        },
        Store::open_in_memory().unwrap(),
        AppConfig::default(),
        &adapters,
    )
    .unwrap();
    (job, adapters)
}

fn synced_source_ids(job: &SyncJob) -> Vec<String> {
    let mut ids = job
        .store
        .session_paths_for_source("test")
        .unwrap()
        .into_iter()
        .map(|path| path.source_id)
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

#[test]
fn scoped_sync_writes_only_sessions_inside_the_scope() {
    let (mut job, adapters) = scoped_job(ProjectScope::Directory("/repo/root".to_string()));

    job.run_with(&adapters, None).unwrap();

    assert_eq!(synced_source_ids(&job), vec!["inside".to_string()]);
    assert_eq!(job.stats.out_of_scope, 1);
}

#[test]
fn scoped_sync_leaves_sessions_outside_the_scope_untouched() {
    let (mut job, adapters) = scoped_job(ProjectScope::Directory("/repo/root".to_string()));
    let mut existing = session("s-outside", "test", "outside");
    existing.directory = Some("/elsewhere".to_string());
    existing.message_count = 7;
    job.store.insert_session(&existing).unwrap();

    job.run_with(&adapters, None).unwrap();

    assert_eq!(job.store.session_meta("test", "outside").unwrap(), Some((Some(1), 7)));
}

#[test]
fn failed_scan_preserves_existing_sessions() {
    schema::register_sqlite_vec();
    let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(FailingAdapter)];
    let mut job = SyncJob::new(
        SyncRunOptions {
            force: false,
            verbose: false,
            emit: false,
            usage_only: false,
            backfill_events: false,
            sources: None,
            scope: ProjectScope::Global,
            target_session: None,
        },
        Store::open_in_memory().unwrap(),
        AppConfig::default(),
        &adapters,
    )
    .unwrap();
    job.store.insert_session(&session("stale", "test", "stale")).unwrap();

    job.run_with(&adapters, None).unwrap();

    assert!(job.store.session_meta("test", "stale").unwrap().is_some());
}

#[test]
fn failed_file_parse_does_not_apply_source_observation() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(ObservationAdapter {
        directory: None,
        include_session: false,
        failing_path: Some(file.path().to_path_buf()),
    })];
    let mut job = global_job(&adapters);
    let mut imported = session("imported", "test", "observed");
    imported.is_import = true;
    job.store.insert_session(&imported).unwrap();

    job.run_with(&adapters, None).unwrap();

    let stored = job.store.list_recent_sessions(10).unwrap().pop().unwrap();
    assert!(stored.is_import);
    assert!(stored.source_file_path.is_none());
}

#[test]
fn successful_skipped_observation_updates_path_and_clears_import_marker() {
    let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(ObservationAdapter {
        directory: None,
        include_session: false,
        failing_path: None,
    })];
    let mut job = global_job(&adapters);
    let mut imported = session("imported", "test", "observed");
    imported.is_import = true;
    job.store.insert_session(&imported).unwrap();

    job.run_with(&adapters, None).unwrap();

    let stored = job.store.list_recent_sessions(10).unwrap().pop().unwrap();
    assert!(!stored.is_import);
    assert_eq!(stored.source_file_path.as_deref(), Some("/tmp/observed.jsonl"));
}

#[test]
fn scoped_observation_without_directory_uses_stored_session_scope() {
    let scopes = [
        ProjectScope::Directory("/repo/root".to_string()),
        ProjectScope::Repository {
            filter: RepoFilter::Name("unmatched".to_string()),
            local_root: Some("/repo/root".to_string()),
        },
    ];
    for scope in scopes {
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(ObservationAdapter {
            directory: None,
            include_session: false,
            failing_path: None,
        })];
        let (mut job, _) = scoped_job(scope);
        let mut imported = session("imported", "test", "observed");
        imported.directory = Some("/repo/root/project".to_string());
        imported.is_import = true;
        job.store.insert_session(&imported).unwrap();

        job.run_with(&adapters, None).unwrap();

        let stored = job.store.list_recent_sessions(10).unwrap().pop().unwrap();
        assert!(!stored.is_import);
        assert_eq!(stored.source_file_path.as_deref(), Some("/tmp/observed.jsonl"));
    }
}

#[test]
fn skipped_observation_applies_path_exclusion_after_success() {
    schema::register_sqlite_vec();
    let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(ObservationAdapter {
        directory: None,
        include_session: false,
        failing_path: None,
    })];
    let mut config = AppConfig::default();
    config.excluded_paths = vec!["**/observed.jsonl".to_string()];
    let mut job = SyncJob::new(
        SyncRunOptions {
            force: false,
            verbose: false,
            emit: false,
            usage_only: false,
            backfill_events: false,
            sources: None,
            scope: ProjectScope::Global,
            target_session: None,
        },
        Store::open_in_memory().unwrap(),
        config,
        &adapters,
    )
    .unwrap();
    job.store.insert_session(&session("existing", "test", "observed")).unwrap();

    job.run_with(&adapters, None).unwrap();

    assert!(job.store.session_meta("test", "observed").unwrap().is_none());
}

#[test]
fn out_of_scope_observation_does_not_update_existing_session() {
    let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(ObservationAdapter {
        directory: Some("/elsewhere"),
        include_session: false,
        failing_path: None,
    })];
    let (mut job, _) = scoped_job(ProjectScope::Directory("/repo/root".to_string()));
    let mut imported = session("imported", "test", "observed");
    imported.directory = Some("/elsewhere".to_string());
    imported.is_import = true;
    job.store.insert_session(&imported).unwrap();

    job.run_with(&adapters, None).unwrap();

    let stored = job.store.list_recent_sessions(10).unwrap().pop().unwrap();
    assert!(stored.is_import);
    assert!(stored.source_file_path.is_none());
}

#[test]
fn session_processing_failure_does_not_apply_source_observation() {
    let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(ObservationAdapter {
        directory: None,
        include_session: true,
        failing_path: None,
    })];
    let mut job = global_job(&adapters);
    let mut imported = session("imported", "test", "observed");
    imported.is_import = true;
    job.store.insert_session(&imported).unwrap();
    job.store.conn.execute("DROP TABLE messages", []).unwrap();

    assert!(job.run_with(&adapters, None).is_err());

    let stored = job.store.list_recent_sessions(10).unwrap().pop().unwrap();
    assert!(stored.is_import);
    assert!(stored.source_file_path.is_none());
}

#[test]
fn complete_live_set_deletes_only_stale_sessions() {
    for force in [false, true] {
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(ReconcileAdapter {
            plan: ReconcilePlan::CompleteLiveSet(HashSet::from(["keep".to_string()])),
            include_session: false,
        })];
        let (mut job, _) = scoped_job(ProjectScope::Global);
        job.options.force = force;
        job.store.insert_session(&session("keep", "test", "keep")).unwrap();
        job.store.insert_session(&session("stale", "test", "stale")).unwrap();

        job.run_with(&adapters, None).unwrap();

        assert!(job.store.session_meta("test", "keep").unwrap().is_some());
        assert!(job.store.session_meta("test", "stale").unwrap().is_none());
    }
}

#[test]
fn complete_live_set_keeps_session_written_during_same_source_run() {
    let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(ReconcileAdapter {
        plan: ReconcilePlan::CompleteLiveSet(HashSet::from(["new".to_string()])),
        include_session: true,
    })];
    let (mut job, _) = scoped_job(ProjectScope::Global);
    job.store.insert_session(&session("stale", "test", "stale")).unwrap();

    job.run_with(&adapters, None).unwrap();

    assert!(job.store.session_meta("test", "new").unwrap().is_some());
    assert!(job.store.session_meta("test", "stale").unwrap().is_none());
}

#[test]
fn complete_exact_tombstone_deletes_owned_session() {
    let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(ReconcileAdapter {
        plan: ReconcilePlan::ExactTombstones(HashSet::from(["subagent".to_string()])),
        include_session: false,
    })];
    let (mut job, _) = scoped_job(ProjectScope::Global);
    job.store.insert_session(&session("subagent", "test", "subagent")).unwrap();

    job.run_with(&adapters, None).unwrap();

    assert!(job.store.session_meta("test", "subagent").unwrap().is_none());
}

#[test]
fn partial_inventory_preserves_existing_sessions() {
    let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(ReconcileAdapter {
        plan: ReconcilePlan::PartialInventory(vec![InventoryIssue {
            path: "/unreadable".into(),
            category: std::io::ErrorKind::PermissionDenied,
        }]),
        include_session: false,
    })];
    let (mut job, _) = scoped_job(ProjectScope::Global);
    job.store.insert_session(&session("stale", "test", "stale")).unwrap();

    job.run_with(&adapters, None).unwrap();

    assert!(job.store.session_meta("test", "stale").unwrap().is_some());
}

#[test]
fn scoped_sync_cannot_apply_complete_reconcile_plan() {
    let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(ReconcileAdapter {
        plan: ReconcilePlan::CompleteLiveSet(HashSet::new()),
        include_session: false,
    })];
    let (mut job, _) = scoped_job(ProjectScope::Directory("/repo/root".to_string()));
    let mut existing = session("stale", "test", "stale");
    existing.directory = Some("/repo/root".to_string());
    job.store.insert_session(&existing).unwrap();

    job.run_with(&adapters, None).unwrap();

    assert!(job.store.session_meta("test", "stale").unwrap().is_some());
}

#[test]
fn session_processing_failure_does_not_apply_reconcile_plan() {
    let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(ReconcileAdapter {
        plan: ReconcilePlan::CompleteLiveSet(HashSet::new()),
        include_session: true,
    })];
    let (mut job, _) = scoped_job(ProjectScope::Global);
    job.store.insert_session(&session("stale", "test", "stale")).unwrap();
    job.store.conn.execute("DROP TABLE messages", []).unwrap();

    assert!(job.run_with(&adapters, None).is_err());
    assert!(job.store.session_meta("test", "stale").unwrap().is_some());
}

#[test]
fn repository_scope_falls_back_to_local_root_when_identity_is_unknown() {
    let (mut job, adapters) = scoped_job(ProjectScope::Repository {
        filter: crate::db::search::RepoFilter::Remote("github.com/samzong/Recall".to_string()),
        local_root: Some("/repo/root".to_string()),
    });

    job.run_with(&adapters, None).unwrap();

    assert_eq!(synced_source_ids(&job), vec!["inside".to_string()]);
}

#[test]
fn sync_job_refresh_preserves_session_id_and_replaces_indexed_content() {
    let initial: Vec<Box<dyn SourceAdapter>> = vec![Box::new(StaticAdapter {
        updated_at: 2_000,
        messages: &["old content"],
        source_file_path: None,
        optimized: false,
    })];
    let mut job = global_job(&initial);

    job.run_with(&initial, None).unwrap();
    let original = job.store.list_recent_sessions(1).unwrap().pop().unwrap();

    let updated: Vec<Box<dyn SourceAdapter>> = vec![Box::new(StaticAdapter {
        updated_at: 3_000,
        messages: &["new content"],
        source_file_path: None,
        optimized: false,
    })];
    job.run_with(&updated, None).unwrap();

    let refreshed = job.store.list_recent_sessions(1).unwrap().pop().unwrap();
    assert_eq!(refreshed.id, original.id);
    assert_eq!(job.store.get_messages(&refreshed.id).unwrap()[0].content, "new content");
}

#[test]
fn imported_session_takeover_preserves_session_id_and_replaces_content() {
    let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(StaticAdapter {
        updated_at: 3_000,
        messages: &["sourceownedtoken"],
        source_file_path: None,
        optimized: false,
    })];
    let mut job = global_job(&adapters);
    let mut imported = session("imported-id", "test", "raw1");
    imported.updated_at = Some(2_000);
    imported.message_count = 1;
    imported.is_import = true;
    job.store.insert_session(&imported).unwrap();
    job.store
        .insert_messages(&[Message {
            session_id: imported.id.clone(),
            role: Role::User,
            content: "importeduniquetoken".to_string(),
            timestamp: Some(2_000),
            seq: 0,
        }])
        .unwrap();

    job.run_with(&adapters, None).unwrap();

    let refreshed = job.store.list_recent_sessions(1).unwrap().pop().unwrap();
    assert_eq!(refreshed.id, "imported-id");
    assert!(!refreshed.is_import);
    assert_eq!(job.store.get_messages(&refreshed.id).unwrap()[0].content, "sourceownedtoken");
}

#[test]
fn source_path_backfill_runs_after_scope_and_before_time_filter() {
    schema::register_sqlite_vec();
    let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(StaticAdapter {
        updated_at: 2_000,
        messages: &[],
        source_file_path: Some("/tmp/session.jsonl"),
        optimized: true,
    })];
    let mut config = AppConfig::default();
    config.sync_window = crate::config::SyncWindow::Today;
    let mut global_job = SyncJob::new(
        SyncRunOptions {
            force: false,
            verbose: false,
            emit: false,
            usage_only: false,
            backfill_events: false,
            sources: None,
            scope: ProjectScope::Global,
            target_session: None,
        },
        Store::open_in_memory().unwrap(),
        config,
        &adapters,
    )
    .unwrap();
    global_job.store.insert_session(&session("global", "test", "raw1")).unwrap();

    global_job.run_with(&adapters, None).unwrap();

    assert_eq!(
        global_job.store.session_paths_for_source("test").unwrap()[0].source_file_path.as_deref(),
        Some("/tmp/session.jsonl")
    );

    let mut scoped_job = SyncJob::new(
        SyncRunOptions {
            force: false,
            verbose: false,
            emit: false,
            usage_only: false,
            backfill_events: false,
            sources: None,
            scope: ProjectScope::Directory("/repo/root".to_string()),
            target_session: None,
        },
        Store::open_in_memory().unwrap(),
        AppConfig::default(),
        &adapters,
    )
    .unwrap();
    scoped_job.store.insert_session(&session("scoped", "test", "raw1")).unwrap();

    scoped_job.run_with(&adapters, None).unwrap();

    assert_eq!(
        scoped_job.store.session_paths_for_source("test").unwrap()[0].source_file_path,
        None
    );
}

#[test]
fn delete_excluded_sessions_for_source_uses_persisted_source_file_path() {
    schema::register_sqlite_vec();
    let matcher = matcher("**/observer-sessions");
    let store = Store::open_in_memory().unwrap();
    store.insert_session(&session("id-1", "claude-code", "s1")).unwrap();
    store
        .update_session_fields(
            "claude-code",
            "s1",
            None,
            None,
            None,
            Some("/tmp/observer-sessions/session.jsonl"),
        )
        .unwrap();

    let mut deleted = HashSet::new();
    let count = delete_excluded_sessions_for_source(
        &store,
        "claude-code",
        &matcher,
        &ProjectScope::Global,
        None,
        &mut deleted,
    )
    .unwrap();

    assert_eq!(count, 1);
    assert!(deleted.contains("s1"));
    assert!(store.session_paths_for_source("claude-code").unwrap().is_empty());
}

#[test]
fn excluded_source_file_path_blocks_fresh_and_force_sync() {
    for force in [false, true] {
        schema::register_sqlite_vec();
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(StaticAdapter {
            updated_at: 2_000,
            messages: &[],
            source_file_path: Some("/tmp/private-sessions/session.jsonl"),
            optimized: true,
        })];
        let mut config = AppConfig::default();
        config.excluded_paths = vec!["**/private-sessions".to_string()];
        let mut job = SyncJob::new(
            SyncRunOptions {
                force,
                verbose: false,
                emit: false,
                usage_only: false,
                backfill_events: false,
                sources: None,
                scope: ProjectScope::Global,
                target_session: None,
            },
            Store::open_in_memory().unwrap(),
            config,
            &adapters,
        )
        .unwrap();

        job.run_with(&adapters, None).unwrap();

        assert!(job.store.session_paths_for_source("test").unwrap().is_empty());
        assert_eq!(job.stats.excluded_out, 1);
    }
}

#[test]
fn source_progress_reports_ids_that_are_actually_scanned() {
    let (mut job, adapters) = scoped_job(ProjectScope::Global);
    let mut seen = Vec::new();
    job.run_with(&adapters, Some(&mut |source| seen.push(source.to_string()))).unwrap();
    assert_eq!(seen, ["test"]);
}

#[test]
fn source_progress_skips_adapters_without_usage_during_usage_sync() {
    let (mut job, adapters) = scoped_job(ProjectScope::Global);
    job.options.usage_only = true;
    let mut seen = Vec::new();
    job.run_with(&adapters, Some(&mut |label| seen.push(label.to_string()))).unwrap();
    assert!(seen.is_empty());
}

#[test]
fn single_session_sync_writes_only_its_target() {
    let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(ReconcileAdapter {
        plan: ReconcilePlan::CompleteLiveSet(HashSet::from(["new".to_string()])),
        include_session: true,
    })];
    for target in ["new", "kept"] {
        let mut job = single_session_job(&adapters, target);
        job.store.insert_session(&session("kept", "test", "kept")).unwrap();
        job.store.insert_session(&session("existing", "test", "new")).unwrap();
        job.run_with(&adapters, None).unwrap();
        let indexed = job.store.session_meta_map("test").unwrap();
        assert_eq!(indexed["kept"].message_count, 0);
        assert_eq!(indexed["new"].message_count, u32::from(target == "new"));
    }
}

#[test]
fn single_session_scan_failure_preserves_existing_session() {
    let temp = tempfile::tempdir().unwrap();
    for adapter in [
        Box::new(FailingAdapter) as Box<dyn SourceAdapter>,
        Box::new(ObservationAdapter {
            directory: None,
            include_session: false,
            failing_path: Some(temp.path().join("missing.jsonl")),
        }),
    ] {
        let adapters = vec![adapter];
        let mut job = single_session_job(&adapters, "observed");
        let mut existing = session("retained", "test", "observed");
        existing.source_file_path =
            Some(temp.path().join("missing.jsonl").to_string_lossy().into());
        job.path_excluder = Some(matcher(existing.source_file_path.as_deref().unwrap()));
        job.store.insert_session(&existing).unwrap();
        assert!(job.run_with(&adapters, None).is_err());
        assert_eq!(
            job.store.get_native_session("test", "observed").unwrap().unwrap().id,
            "retained"
        );
    }
}
