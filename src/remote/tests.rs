use super::*;
use crate::db::remote_store::{Revision, digest, revision_heads};
use crate::extension::transport::{Object, Page};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

fn seed(store: &Store, text: &str, native: bool) -> String {
    let record = serde_json::json!({
        "schema_version": 7, "record_type": "session",
        "session": {"source":"codex", "source_id":"shared-native-id", "title":"Synthetic remote fixture", "started_at":100, "directory":"/synthetic/project", "source_file_path":"/synthetic/session.jsonl"},
        "messages":[{"seq":0,"role":"user","timestamp":100,"content":text}],
        "usage_events":[{"event_key":"usage-1","event_seq":0,"timestamp":100,"model":"synthetic","provider":"test","input_tokens":3,"output_tokens":2,"cache_read_tokens":0,"cache_write_tokens":0,"reasoning_tokens":0,"token_source":"observed"}],
        "events":[{"event_seq":0,"kind":"tool","actor":"assistant","name":"synthetic-tool","summary":"synthetic event"}]
    });
    crate::import::import_jsonl(store, false, record.to_string().as_bytes()).unwrap();
    if native {
        store.clear_import_marker("codex", "shared-native-id").unwrap();
    }
    store.get_session_by_source_id("codex", "shared-native-id").unwrap().unwrap().id
}

fn transfer(store: &Store, cloud: &mut BTreeMap<String, Vec<u8>>) -> Result<SyncSummary> {
    transfer_observing(store, cloud, &|_, _| Ok(()))
}

fn transfer_observing(
    store: &Store,
    cloud: &mut BTreeMap<String, Vec<u8>>,
    before_get: &(dyn Fn(&str, &std::path::Path) -> Result<()> + Sync),
) -> Result<SyncSummary> {
    let cloud = Mutex::new(cloud);
    exchange(
        store,
        &Connection { provider: "r2".into(), scope: ProjectScope::Global },
        &mut crate::sync_progress::SyncProgress::disabled(),
        &|operation| {
            if let Operation::Get { key, output_path, .. } = operation {
                ensure!(!output_path.exists(), "download path already exists");
                before_get(key, output_path)?;
            }
            let mut cloud = cloud.lock().expect("synthetic cloud");
            Ok(match operation {
                Operation::List { cursor, .. } => {
                    if cursor.is_none() {
                        Reply::Listed(Page {
                            objects: Vec::new(),
                            next_cursor: Some("nonempty-next-page".into()),
                        })
                    } else {
                        Reply::Listed(Page {
                            objects: cloud
                                .iter()
                                .map(|(key, value)| Object {
                                    key: key.clone(),
                                    size: value.len() as u64,
                                })
                                .collect(),
                            next_cursor: None,
                        })
                    }
                }
                Operation::Get { key, output_path, .. } => {
                    let body = cloud.get(key).context("synthetic object missing")?;
                    std::fs::write(output_path, body)?;
                    Reply::Downloaded(body.len() as u64)
                }
                Operation::Put { key, input_path, size, sha256 } => {
                    let body = std::fs::read(input_path)?;
                    ensure!(body.len() as u64 == *size && digest(&body) == *sha256, "bad upload");
                    if let Some(existing) = cloud.get(key) {
                        ensure!(existing == &body, "immutable overwrite");
                    }
                    cloud.insert(key.clone(), body);
                    Reply::Published
                }
                Operation::Probe => Reply::Readable,
            })
        },
    )
}

#[test]
fn remote_large_revision_preserves_complete_events_and_recovers_objects() {
    crate::db::schema::register_sqlite_vec();
    let a = Store::open_in_memory().unwrap();
    let b = Store::open_in_memory().unwrap();
    let id = seed(&a, "large event snapshot", true);
    let attrs = serde_json::json!({"payload": "x".repeat(64 * 1024 * 1024 + 1)}).to_string();
    a.conn
        .execute(
            "UPDATE session_events SET attrs_json = ?1 WHERE session_id = ?2",
            rusqlite::params![attrs, id],
        )
        .unwrap();
    let mut cloud = BTreeMap::new();
    transfer(&a, &mut cloud).unwrap();
    assert!(cloud.values().any(|body| body.len() > 64 * 1024 * 1024));
    transfer(&b, &mut cloud).unwrap();
    let received = b.list_recent_sessions(1).unwrap().pop().unwrap();
    let actual: String = b
        .conn
        .query_row(
            "SELECT attrs_json FROM session_events WHERE session_id = ?1",
            [&received.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(actual, attrs);
    assert_eq!(transfer(&b, &mut cloud).unwrap().uploaded, 0);
    let key = cloud.iter().max_by_key(|(_, body)| body.len()).unwrap().0.clone();
    let lost = cloud.remove(&key).unwrap();
    assert_eq!(transfer(&b, &mut cloud).unwrap().uploaded, 1);
    assert_eq!(cloud[&key], lost);
}

#[test]
fn remote_roundtrip_updates_replicas_without_native_ownership_and_recovers_objects() {
    crate::db::schema::register_sqlite_vec();
    let a = Store::open_in_memory().unwrap();
    let b = Store::open_in_memory().unwrap();
    let a_id = seed(&a, "original synthetic message", true);
    let mut host =
        Host { id: uuid::Uuid::new_v4().to_string(), name: "synthetic-a".into(), revision: 1 };
    host.observe(&a.conn, "codex", "shared-native-id").unwrap();
    let mut cloud = BTreeMap::new();
    assert!(transfer(&a, &mut cloud).unwrap().uploaded > 0);
    transfer(&b, &mut cloud).unwrap();
    let b_session = b.list_recent_sessions(10).unwrap().pop().unwrap();
    assert_ne!(a_id, b_session.id);
    assert!(!b.has_native_binding(&b_session.id).unwrap());
    assert_eq!(
        b.remote_snapshot(b_session.clone()).unwrap(),
        a.remote_snapshot(a.get_session_by_id(&a_id).unwrap().unwrap()).unwrap()
    );
    assert_eq!(b_session.locations[0].host, host);
    assert_eq!(b.list_usage_events_for_session(&b_session.id).unwrap().len(), 1);
    assert_eq!(b.list_session_events_for_session(&b_session.id).unwrap().len(), 1);
    let count = a.session_revisions(&a_id).unwrap().len();
    host.name = "renamed-a".into();
    host.revision += 1;
    host.observe(&a.conn, "codex", "shared-native-id").unwrap();
    transfer(&a, &mut cloud).unwrap();
    transfer(&b, &mut cloud).unwrap();
    assert_eq!(a.session_revisions(&a_id).unwrap().len(), count);
    assert_eq!(b.get_session_by_id(&b_session.id).unwrap().unwrap().locations[0].host, host);
    a.conn
        .execute(
            "UPDATE messages SET content = 'updated synthetic message' WHERE session_id = ?1",
            [&a_id],
        )
        .unwrap();
    transfer(&a, &mut cloud).unwrap();
    transfer(&b, &mut cloud).unwrap();
    assert_eq!(b.get_messages(&b_session.id).unwrap()[0].content, "updated synthetic message");
    assert_eq!(transfer(&b, &mut cloud).unwrap().uploaded, 0);
    let lost = cloud.keys().find(|key| key.starts_with("v1/revisions/")).unwrap().clone();
    let expected = cloud.remove(&lost).unwrap();
    assert_eq!(transfer(&b, &mut cloud).unwrap().uploaded, 1);
    assert_eq!(cloud[&lost], expected);
    assert_eq!(b.stats().unwrap().0, 1);
}

#[test]
fn remote_downloads_overlap_retry_transient_gets_and_stop_on_a_failed_get() {
    crate::db::schema::register_sqlite_vec();
    let mut cloud = BTreeMap::new();
    for text in ["parallel left", "parallel right", "parallel middle"] {
        let origin = Store::open_in_memory().unwrap();
        seed(&origin, text, true);
        transfer(&origin, &mut cloud).unwrap();
    }
    assert!(cloud.len() > 1);

    let in_flight = AtomicUsize::new(0);
    let peak = AtomicUsize::new(0);
    let b = Store::open_in_memory().unwrap();
    let summary = transfer_observing(&b, &mut cloud, &|_, _| {
        peak.fetch_max(in_flight.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(50));
        in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(())
    })
    .unwrap();
    assert_eq!(summary.downloaded, cloud.len());
    assert!(peak.load(Ordering::SeqCst) > 1);

    let broken = cloud.keys().next().unwrap().clone();
    let transient = || {
        anyhow::Error::from(crate::extension::transport::RemoteError {
            code: crate::extension::transport::ErrorCode::Transient,
            message: "synthetic stall".into(),
        })
    };
    let attempts = AtomicUsize::new(0);
    let d = Store::open_in_memory().unwrap();
    let summary = transfer_observing(&d, &mut cloud, &|key, output_path| {
        if key == broken && attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            std::fs::write(output_path, b"partial")?;
            return Err(transient());
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(summary.downloaded, cloud.len());
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    attempts.store(0, Ordering::SeqCst);
    let e = Store::open_in_memory().unwrap();
    let stalled = transfer_observing(&e, &mut cloud, &|key, _| {
        if key == broken {
            attempts.fetch_add(1, Ordering::SeqCst);
            return Err(transient());
        }
        Ok(())
    });
    assert!(format!("{:#}", stalled.err().unwrap()).contains("synthetic stall"));
    assert_eq!(attempts.load(Ordering::SeqCst), 3);

    let c = Store::open_in_memory().unwrap();
    let failed = transfer_observing(&c, &mut cloud, &|key, _| {
        ensure!(key != broken, "synthetic get failure");
        Ok(())
    });
    assert!(format!("{:#}", failed.err().unwrap()).contains("synthetic get failure"));
    assert_eq!(c.cached_remote_size(&broken).unwrap(), None);
}

#[test]
fn remote_legacy_exact_match_propagates_association_and_keeps_independent_id_collision() {
    crate::db::schema::register_sqlite_vec();
    let a = Store::open_in_memory().unwrap();
    let b = Store::open_in_memory().unwrap();
    let c = Store::open_in_memory().unwrap();
    let a_id = seed(&a, "same exported message", true);
    let b_id = seed(&b, "same exported message", false);
    let mut cloud = BTreeMap::new();
    transfer(&b, &mut cloud).unwrap();
    transfer(&c, &mut cloud).unwrap();
    transfer(&a, &mut cloud).unwrap();
    transfer(&b, &mut cloud).unwrap();
    transfer(&c, &mut cloud).unwrap();
    transfer(&a, &mut cloud).unwrap();
    assert_eq!(a.stats().unwrap().0, 1);
    assert_eq!(b.stats().unwrap().0, 1);
    assert_eq!(c.stats().unwrap().0, 1);
    assert!(a.has_native_binding(&a_id).unwrap());
    assert!(b.get_session_by_id(&b_id).unwrap().is_some());
    a.conn
        .execute(
            "UPDATE messages SET content = 'subsequent source update' WHERE session_id = ?1",
            [&a_id],
        )
        .unwrap();
    transfer(&a, &mut cloud).unwrap();
    transfer(&b, &mut cloud).unwrap();
    assert_eq!(b.get_messages(&b_id).unwrap()[0].content, "subsequent source update");
    let independent = Store::open_in_memory().unwrap();
    seed(&independent, "independent same-native-id session", true);
    transfer(&independent, &mut cloud).unwrap();
    transfer(&a, &mut cloud).unwrap();
    assert_eq!(a.stats().unwrap().0, 2);
    assert_eq!(a.get_native_session("codex", "shared-native-id").unwrap().unwrap().id, a_id);
    assert!(a.get_session_by_source_id("codex", "shared-native-id").is_err());
}

#[test]
fn remote_concurrent_revisions_keep_one_projection_and_all_full_snapshots() {
    crate::db::schema::register_sqlite_vec();
    let source = Store::open_in_memory().unwrap();
    let receiver = Store::open_in_memory().unwrap();
    seed(&source, "base revision", true);
    let mut cloud = BTreeMap::new();
    transfer(&source, &mut cloud).unwrap();
    transfer(&receiver, &mut cloud).unwrap();
    let original = cloud.iter().find(|(key, _)| key.starts_with("v1/revisions/")).unwrap();
    let parent = digest(original.1);
    let body = original.1.clone();
    for text in ["concurrent left", "concurrent right"] {
        let mut revision: Revision = serde_json::from_slice(&body).unwrap();
        revision.parent = Some(parent.clone());
        revision.record["messages"][0]["content"] = text.into();
        let body = serde_json::to_vec(&revision).unwrap();
        cloud.insert(format!("v1/revisions/{}.json", digest(&body)), body);
    }
    assert_eq!(transfer(&receiver, &mut cloud).unwrap().sessions_with_alternatives, 1);
    let session = receiver.list_recent_sessions(10).unwrap().pop().unwrap();
    assert_eq!(receiver.stats().unwrap().0, 1);
    assert_eq!(receiver.get_messages(&session.id).unwrap()[0].content, "base revision");
    let revisions = receiver.session_revisions(&session.id).unwrap();
    assert_eq!(revisions.len(), 3);
    assert_eq!(revision_heads(&revisions).len(), 2);
    assert_eq!(receiver.list_usage_events_for_session(&session.id).unwrap().len(), 1);
    assert_eq!(session.alternative_versions, 2);
    let fresh = Store::open_in_memory().unwrap();
    transfer(&fresh, &mut cloud).unwrap();
    let fresh_session = fresh.list_recent_sessions(10).unwrap().pop().unwrap();
    assert!(fresh.get_messages(&fresh_session.id).unwrap()[0].content.starts_with("concurrent "));
    assert_eq!(fresh_session.alternative_versions, 1);
}

#[test]
fn remote_objects_are_stored_compressed_and_legacy_raw_rows_stay_readable() {
    crate::db::schema::register_sqlite_vec();
    let a = Store::open_in_memory().unwrap();
    seed(&a, "compressed synthetic message", true);
    let mut cloud = BTreeMap::new();
    transfer(&a, &mut cloud).unwrap();
    assert!(!cloud.is_empty());
    for (key, body) in &cloud {
        let (table, hash) = key.trim_end_matches(".json").rsplit_once('/').unwrap();
        let table = if table == "v1/revisions" { "sync_revisions" } else { "sync_objects" };
        let stored: Vec<u8> = a
            .conn
            .query_row(&format!("SELECT body FROM {table} WHERE digest = ?1"), [hash], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(stored.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]));
        assert_eq!(a.cached_remote(key).unwrap().as_ref(), Some(body));
        assert_eq!(a.cached_remote_size(key).unwrap(), Some(body.len() as u64));
    }

    let legacy = Store::open_in_memory().unwrap();
    for (key, body) in &cloud {
        let hash = key.trim_end_matches(".json").rsplit_once('/').unwrap().1;
        if key.starts_with("v1/revisions/") {
            let revision: Revision = serde_json::from_slice(body).unwrap();
            legacy
                .conn
                .execute(
                    "INSERT INTO sync_revisions(digest, sync_id, body) VALUES (?1, ?2, ?3)",
                    rusqlite::params![hash, revision.sync_id, body],
                )
                .unwrap();
        } else {
            legacy
                .conn
                .execute(
                    "INSERT INTO sync_objects(digest, body) VALUES (?1, ?2)",
                    rusqlite::params![hash, body],
                )
                .unwrap();
        }
        assert_eq!(legacy.cached_remote(key).unwrap().as_ref(), Some(body));
        assert_eq!(legacy.cached_remote_size(key).unwrap(), Some(body.len() as u64));
    }
    let summary = transfer(&legacy, &mut cloud).unwrap();
    assert_eq!((summary.downloaded, summary.uploaded), (0, 0));
    let session = legacy.list_recent_sessions(10).unwrap().pop().unwrap();
    assert_eq!(
        legacy.get_messages(&session.id).unwrap()[0].content,
        "compressed synthetic message"
    );
}

#[test]
fn saved_provider_names_roundtrip_without_requiring_an_installed_extension() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("remote.json");
    for provider in ["r2", "opendal"] {
        let settings = Settings {
            host: Host {
                id: uuid::Uuid::new_v4().to_string(),
                name: "workstation".into(),
                revision: 1,
            },
            connection: Some(Connection { provider: provider.into(), scope: ProjectScope::Global }),
        };
        settings.save_to(&path).unwrap();
        assert_eq!(load_from(&path).unwrap().unwrap().connection.unwrap().provider, provider);
    }
    let mut settings = load_from(&path).unwrap().unwrap();
    settings.connection.as_mut().unwrap().provider = "../opendal".into();
    settings.save_to(&path).unwrap();
    assert!(load_from(&path).is_err());
}
