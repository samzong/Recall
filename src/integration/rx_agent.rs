use std::fs;
use std::io::Write;

use crate::adapters::file_scan::FileScanEntry;
use crate::adapters::rx_agent::parse_session;
use crate::adapters::{SourceAdapter, rx_agent::RxAgentAdapter};
use crate::types::FileOperation;

const ID: &str = "019a0000-0000-7000-8000-000000000001";
const FIXTURE: &str =
    include_str!("../../tests/fixtures/rx-agent/019a0000-0000-7000-8000-000000000001.jsonl");

fn entry(root: &std::path::Path, data: &str) -> FileScanEntry {
    let path = root.join(format!("{ID}.jsonl"));
    fs::write(&path, data).unwrap();
    FileScanEntry { session_id: ID.into(), stat_target: path, directory: None }
}

#[test]
fn rx_agent_keeps_history_usage_and_file_events_separate() {
    let root = tempfile::tempdir().unwrap();
    let entry = entry(root.path(), FIXTURE);
    let raw = parse_session(&entry, 1_792_922_410_000, true).unwrap().unwrap();
    assert_eq!(raw.source_id, ID);
    assert_eq!(raw.directory.as_deref(), Some("/fixture/rx-project"));
    assert_eq!(raw.started_at, 1_792_922_400_000);
    assert_eq!(raw.messages.len(), 4);
    assert_eq!(raw.messages[0].content, "Inspect the rx adapter contract");
    assert_eq!(raw.messages[2].timestamp, Some(1_792_922_405_000));
    assert_eq!(raw.usage_events.len(), 3);
    assert_eq!(raw.usage_events.iter().map(|event| event.input_tokens).sum::<i64>(), 170);
    assert_eq!(raw.usage_events[0].model, "fixture-model");
    assert_eq!(raw.usage_events[0].cache_read_tokens, 30);
    assert_eq!(raw.usage_events[1].message_seq, None);
    assert_eq!(raw.events.len(), 4);
    assert_eq!(raw.events[0].files[0].operation, FileOperation::Read);
    assert_eq!(raw.events[2].files[0].operation, FileOperation::Write);
    assert_eq!(raw.events[3].status.as_deref(), Some("error"));
    assert!(raw.parent_links.is_empty());
    assert!(raw.messages.iter().all(|message| !message.content.contains("summary")
        && !message.content.contains("payload")
        && !message.content.contains("reasoning")
        && !message.content.contains("fixture-image")));
    let no_events = parse_session(&entry, 1_792_922_410_000, false).unwrap().unwrap();
    assert!(no_events.events.is_empty());
    assert_eq!(no_events.messages.len(), raw.messages.len());
    assert_eq!(no_events.usage_events.len(), raw.usage_events.len());
}

#[test]
fn rx_agent_ignores_uncommitted_tail_and_tolerates_bad_message_records() {
    let root = tempfile::tempdir().unwrap();
    let tail = r#"{"type":"message","message":{"role":"user","content":"uncommitted"}}"#;
    let entry = entry(root.path(), &format!("{FIXTURE}invalid-record\n{tail}"));
    let raw = parse_session(&entry, 0, true).unwrap().unwrap();
    assert_eq!(raw.messages.len(), 4);
    fs::OpenOptions::new().append(true).open(&entry.stat_target).unwrap().write_all(b"\n").unwrap();
    let raw = parse_session(&entry, 0, true).unwrap().unwrap();
    assert_eq!(raw.messages.len(), 5);
    assert_eq!(raw.messages[4].content, "uncommitted");
}

#[test]
fn rx_agent_rejects_unknown_version_and_invalid_identity() {
    let root = tempfile::tempdir().unwrap();
    for data in [
        FIXTURE.replacen("\"v\":1", "\"v\":2", 1),
        FIXTURE.replacen(ID, "019a0000-0000-7000-8000-000000000002", 1),
        FIXTURE.replacen("\"cwd\":\"/fixture/rx-project\"", "\"cwd\":null", 1),
        format!("broken-header\n{FIXTURE}"),
    ] {
        assert!(parse_session(&entry(root.path(), &data), 0, true).is_err());
    }
}

#[test]
fn rx_agent_resume_uses_native_session_id() {
    let resume = RxAgentAdapter.resume_command(ID).unwrap();
    assert_eq!(resume.args, ["--resume", ID]);
}
