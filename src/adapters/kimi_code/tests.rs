use std::io::Write;
use std::time::{Duration, UNIX_EPOCH};

use super::*;
use crate::adapters::test_support::{seed_empty_event_state, seed_empty_usage_state};
use crate::db::store::Store;

pub(super) fn fixture_state() -> &'static str {
    r#"{"id":"session_abc","cwd":"/repo","createdAt":1700000000000,"updatedAt":1700000060000,"title":"fix the bug","isCustomTitle":false,"archived":false}"#
}

pub(super) fn fixture_wire() -> &'static str {
    concat!(
        r#"{"type":"metadata","protocol_version":"1.5","created_at":1700000000100}"#,
        "\n",
        r#"{"type":"turn.prompt","time":1700000001000,"origin":{"kind":"user"},"input":[{"type":"text","text":"fix the bug"}]}"#,
        "\n",
        r#"{"type":"context.append_message","time":1700000001001,"message":{"id":"m1","role":"user","origin":{"kind":"user"},"content":[{"type":"text","text":"fix the bug"}]}}"#,
        "\n",
        r#"{"type":"context.append_message","time":1700000002000,"message":{"id":"m2","role":"user","origin":{"kind":"injection"},"content":[{"type":"text","text":"<system-reminder>noise</system-reminder>"}]}}"#,
        "\n",
        r#"{"type":"context.append_message","time":1700000002500,"message":{"id":"m3","role":"user","origin":{"kind":"task"},"content":[{"type":"text","text":"<notification>background task done</notification>"}]}}"#,
        "\n",
        r#"{"type":"llm.request","time":1700000003000,"model":"kimi-k3","provider":"moonshot","kind":"loop"}"#,
        "\n",
        r#"{"type":"context.append_loop_event","time":1700000004000,"event":{"type":"content.part","uuid":"u1","part":{"type":"think","think":"pondering"}}}"#,
        "\n",
        r#"{"type":"context.append_loop_event","time":1700000005000,"event":{"type":"content.part","uuid":"u2","part":{"type":"text","text":"Here is the fix."}}}"#,
        "\n",
        r#"{"type":"context.append_loop_event","time":1700000005500,"event":{"type":"tool.call","uuid":"call-uuid","turnId":"1","step":1,"stepUuid":"step-uuid","toolCallId":"Edit:1","name":"Edit","args":{"path":"DEVELOPMENT.md","old_string":"old","new_string":"new"}}}"#,
        "\n",
        r#"{"type":"context.append_loop_event","time":1700000005600,"event":{"type":"tool.result","parentUuid":"call-uuid","toolCallId":"Edit:1","result":{"output":"Replaced 1 occurrence in DEVELOPMENT.md"}}}"#,
        "\n",
        r#"{"type":"usage.record","time":1700000006000,"model":"kimi-k3","usageScope":"turn","usage":{"inputOther":1200,"output":80,"inputCacheRead":300,"inputCacheCreation":40}}"#,
    )
}

fn write_kimi_fixture(root: &Path) -> (PathBuf, PathBuf) {
    let session_dir = root.join("wd_repo").join("session_abc");
    let wire_dir = session_dir.join("agents").join("main");
    fs::create_dir_all(&wire_dir).unwrap();
    let state_path = session_dir.join("state.json");
    let wire_path = wire_dir.join("wire.jsonl");
    fs::write(&state_path, fixture_state()).unwrap();
    fs::write(&wire_path, fixture_wire()).unwrap();
    (state_path, wire_path)
}

fn append_wire_change(path: &Path) -> std::io::Result<()> {
    fs::OpenOptions::new()
        .append(true)
        .open(path)?
        .write_all(b"\n{\"type\":\"metadata\",\"changed\":true}\n")
}

#[derive(Clone, Copy, Debug)]
enum SnapshotMutation {
    State,
    AppendWire,
    ReplaceState,
}

fn run_kimi_snapshot_scan(
    context: &AdapterSyncContext,
    root: &Path,
    mutation: Option<SnapshotMutation>,
) -> SyncScanResult {
    file_scan::run_file_scan_with_options_and_snapshot(
        context,
        None,
        kimi_scan_options(true),
        collect_session_entries(root),
        kimi_session_snapshot,
        |entry, mtime_ms| {
            let state_path = session_dir_for_wire(&entry.stat_target).unwrap().join("state.json");
            match mutation {
                Some(SnapshotMutation::State) => {
                    fs::write(
                        &state_path,
                        r#"{"id":"session_abc","cwd":"/repo","createdAt":1700000000000,"title":"changed while parsing","isCustomTitle":true}"#,
                    )?;
                    parse_kimi_session_file(entry, mtime_ms, true)
                }
                Some(SnapshotMutation::AppendWire) => {
                    let raw = parse_kimi_session_file(entry.clone(), mtime_ms, true)?;
                    append_wire_change(&entry.stat_target)?;
                    Ok(raw)
                }
                Some(SnapshotMutation::ReplaceState) => {
                    let raw = parse_kimi_session_file(entry, mtime_ms, true)?;
                    let replacement = state_path.with_extension("replacement");
                    fs::write(
                        &replacement,
                        r#"{"id":"session_abc","cwd":"/repo","createdAt":1700000000000,"title":"replacement state with a different length","isCustomTitle":false}"#,
                    )?;
                    fs::remove_file(&state_path)?;
                    fs::rename(replacement, state_path)?;
                    Ok(raw)
                }
                None => parse_kimi_session_file(entry, mtime_ms, true),
            }
        },
    )
    .unwrap()
}

#[test]
fn snapshot_changes_withhold_incremental_candidate_until_stable_retry() {
    for mutation in
        [SnapshotMutation::State, SnapshotMutation::AppendWire, SnapshotMutation::ReplaceState]
    {
        let root = tempfile::tempdir().unwrap();
        write_kimi_fixture(root.path());
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let entry = collect_session_entries(root.path()).pop().unwrap();
        let initial_mtime = kimi_session_snapshot(&entry).unwrap().effective_mtime_ms();
        store
            .conn
            .execute(
                "INSERT INTO sessions (id, source, source_id, title, started_at, updated_at, message_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    "stored-id",
                    "kimi-code",
                    "session_abc",
                    "prior indexed title",
                    1_700_000_000_000_i64,
                    initial_mtime - 1,
                    1,
                ],
            )
            .unwrap();
        store.conn.execute_batch("INSERT INTO native_bindings SELECT source, source_id, id, NOT is_import FROM sessions;").unwrap();

        let result = run_kimi_snapshot_scan(
            &AdapterSyncContext::from_store_for_test(&store, "kimi-code").unwrap(),
            root.path(),
            Some(mutation),
        );
        assert!(result.sessions.is_empty(), "{mutation:?}");
        assert_eq!(result.stats.unstable_sessions, 1, "{mutation:?}");
        let indexed = store.list_recent_sessions(10).unwrap();
        assert_eq!(indexed.len(), 1, "{mutation:?}");
        assert_eq!(indexed[0].title, "prior indexed title", "{mutation:?}");
        assert_eq!(indexed[0].updated_at, Some(initial_mtime - 1), "{mutation:?}");

        let retry = run_kimi_snapshot_scan(
            &AdapterSyncContext::from_store_for_test(&store, "kimi-code").unwrap(),
            root.path(),
            None,
        );
        assert_eq!(retry.sessions.len(), 1, "{mutation:?}");
        assert_eq!(retry.stats.unstable_sessions, 0, "{mutation:?}");
    }
}

#[test]
fn incremental_snapshot_preserves_usage_backfill_and_stable_skip() {
    let root = tempfile::tempdir().unwrap();
    write_kimi_fixture(root.path());
    let entry = collect_session_entries(root.path()).pop().unwrap();
    let mtime_ms = kimi_session_snapshot(&entry).unwrap().effective_mtime_ms();
    crate::db::schema::register_sqlite_vec();
    let store = Store::open_in_memory().unwrap();
    store
        .conn
        .execute(
            "INSERT INTO sessions (id, source, source_id, title, started_at, updated_at, message_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "stored-id",
                "kimi-code",
                "session_abc",
                "existing",
                1_700_000_000_000_i64,
                mtime_ms,
                1,
            ],
        )
        .unwrap();
    store.conn.execute_batch("INSERT INTO native_bindings SELECT source, source_id, id, NOT is_import FROM sessions;").unwrap();
    seed_empty_usage_state(
        &store,
        "kimi-code",
        "session_abc",
        USAGE_PARSER_VERSION - 1,
        Some(mtime_ms),
    );

    let backfill = file_scan::run_file_scan_with_options_and_snapshot(
        &AdapterSyncContext::from_store_for_test(&store, "kimi-code").unwrap(),
        None,
        kimi_scan_options(false),
        collect_session_entries(root.path()),
        kimi_session_snapshot,
        |entry, mtime_ms| parse_kimi_session_file(entry, mtime_ms, false),
    )
    .unwrap();
    assert_eq!(backfill.sessions.len(), 1);
    assert_eq!(backfill.stats.skipped_sessions, 0);
    assert!(backfill.sessions[0].events.is_empty());
    assert_eq!(backfill.sessions[0].event_parser_version, None);

    seed_empty_usage_state(
        &store,
        "kimi-code",
        "session_abc",
        USAGE_PARSER_VERSION,
        Some(mtime_ms),
    );
    let usage_only = scan_kimi_dirs_for_sync(
        &[root.path().to_path_buf()],
        &AdapterSyncContext::from_store_for_test(&store, "kimi-code").unwrap(),
        None,
        false,
    )
    .unwrap();
    assert_eq!(usage_only.stats.skipped_sessions, 1);
    for previous_version in [None, Some(EVENT_PARSER_VERSION - 1)] {
        if let Some(version) = previous_version {
            seed_empty_event_state(&store, "kimi-code", "session_abc", version, Some(mtime_ms));
        }
        let events = scan_kimi_dirs_for_sync(
            &[root.path().to_path_buf()],
            &AdapterSyncContext::from_store_for_test(&store, "kimi-code").unwrap(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(events.sessions.len(), 1);
        assert_eq!(events.sessions[0].events.len(), 2);
    }
    seed_empty_event_state(
        &store,
        "kimi-code",
        "session_abc",
        EVENT_PARSER_VERSION,
        Some(mtime_ms),
    );
    let skipped = file_scan::run_file_scan_with_options_and_snapshot(
        &AdapterSyncContext::from_store_for_test(&store, "kimi-code").unwrap(),
        None,
        kimi_scan_options(true),
        collect_session_entries(root.path()),
        kimi_session_snapshot,
        |_, _| panic!("stable current session must skip parsing"),
    )
    .unwrap();
    assert!(skipped.sessions.is_empty());
    assert_eq!(skipped.stats.skipped_sessions, 1);
}

#[test]
fn full_scan_withholds_candidate_changed_during_parse() {
    let root = tempfile::tempdir().unwrap();
    write_kimi_fixture(root.path());

    let sessions = scan_kimi_session_entries_with_parser(
        collect_session_entries(root.path()),
        |entry, mtime_ms| {
            let raw = parse_kimi_session_file(entry.clone(), mtime_ms, true)?;
            append_wire_change(&entry.stat_target)?;
            Ok(raw)
        },
    )
    .unwrap();

    assert!(sessions.is_empty());
    assert_eq!(
        scan_kimi_session_entries_with_parser(
            collect_session_entries(root.path()),
            |entry, mtime_ms| parse_kimi_session_file(entry, mtime_ms, true),
        )
        .unwrap()
        .len(),
        1
    );
}

#[test]
fn duplicate_session_ids_prefer_the_first_kimi_root() {
    let preferred = tempfile::tempdir().unwrap();
    let fallback = tempfile::tempdir().unwrap();
    write_kimi_fixture(preferred.path());
    let (_, fallback_wire) = write_kimi_fixture(fallback.path());
    fs::write(&fallback_wire, fixture_wire().replace("fix the bug", "fallback")).unwrap();
    let roots = [preferred.path().to_path_buf(), fallback.path().to_path_buf()];

    let sessions = scan_kimi_dirs(&roots).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].messages[0].content, "fix the bug");

    crate::db::schema::register_sqlite_vec();
    let store = Store::open_in_memory().unwrap();
    let result = scan_kimi_dirs_for_sync(
        &roots,
        &AdapterSyncContext::from_store_for_test(&store, "kimi-code").unwrap(),
        None,
        true,
    )
    .unwrap();
    assert_eq!(result.sessions.len(), 1);
    assert_eq!(result.sessions[0].messages[0].content, "fix the bug");
}

#[test]
fn incremental_scan_reparses_when_only_state_changes() {
    let root = tempfile::tempdir().unwrap();
    let session_dir = root.path().join("wd_repo").join("session_rename");
    let wire_dir = session_dir.join("agents").join("main");
    fs::create_dir_all(&wire_dir).unwrap();
    let state_path = session_dir.join("state.json");
    let wire_path = wire_dir.join("wire.jsonl");
    fs::write(
        &state_path,
        r#"{"id":"session_rename","cwd":"/repo","createdAt":1700000000000,"title":"renamed","isCustomTitle":true}"#,
    )
    .unwrap();
    fs::write(
        &wire_path,
        r#"{"type":"context.append_message","time":1700000001000,"message":{"role":"user","origin":{"kind":"user"},"content":[{"type":"text","text":"hello"}]}}"#,
    )
    .unwrap();

    let wire_mtime = 1_700_000_002_000;
    let state_mtime = wire_mtime + 1_000;
    fs::File::open(&wire_path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_millis(wire_mtime as u64))
        .unwrap();
    fs::File::open(&state_path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_millis(state_mtime as u64))
        .unwrap();

    crate::db::schema::register_sqlite_vec();
    let store = Store::open_in_memory().unwrap();
    store
        .conn
        .execute(
            "INSERT INTO sessions (id, source, source_id, title, started_at, updated_at, message_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "stored-id",
                "kimi-code",
                "session_rename",
                "old title",
                1_700_000_000_000_i64,
                wire_mtime,
                1,
            ],
        )
        .unwrap();
    store.conn.execute_batch("INSERT INTO native_bindings SELECT source, source_id, id, NOT is_import FROM sessions;").unwrap();
    seed_empty_usage_state(
        &store,
        "kimi-code",
        "session_rename",
        USAGE_PARSER_VERSION,
        Some(wire_mtime),
    );

    let result = file_scan::run_file_scan_with_options_and_snapshot(
        &AdapterSyncContext::from_store_for_test(&store, "kimi-code").unwrap(),
        None,
        kimi_scan_options(true),
        collect_session_entries(root.path()),
        kimi_session_snapshot,
        |entry, mtime_ms| parse_kimi_session_file(entry, mtime_ms, true),
    )
    .unwrap();

    assert_eq!(result.stats.parsed, 1);
    assert_eq!(result.sessions.len(), 1);
    assert_eq!(result.sessions[0].updated_at, Some(state_mtime));
    assert_eq!(result.sessions[0].custom_title.as_deref(), Some("renamed"));

    let sessions = scan_kimi_session_entries_with_parser(
        collect_session_entries(root.path()),
        |entry, mtime_ms| parse_kimi_session_file(entry, mtime_ms, true),
    )
    .unwrap();
    assert_eq!(sessions[0].updated_at, Some(state_mtime));
}
