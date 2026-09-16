use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use tracing::debug;

use crate::adapters::paths;
use crate::adapters::pi_session::{self, Format, USAGE_PARSER_VERSION, push_existing_unique_dir};
use crate::adapters::{
    AdapterSyncContext, RawSession, ResumeCommand, SourceAdapter, SyncScanResult,
};

pub(crate) struct OmpAdapter;

impl SourceAdapter for OmpAdapter {
    fn id(&self) -> &str {
        "omp"
    }

    fn label(&self) -> &str {
        "OMP"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand::new("omp", &["--resume", source_id]))
    }

    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        Some(crate::adapters::prompt_start("omp", prompt))
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        pi_session::scan(&resolve_omp_session_dirs()?, Format::Omp)
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        Ok(Some(pi_session::scan_for_sync(
            &resolve_omp_session_dirs()?,
            context,
            since_ts,
            include_events,
            Format::Omp,
        )?))
    }
}

fn resolve_omp_session_dirs() -> anyhow::Result<Vec<PathBuf>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let session_dirs = resolve_omp_session_dirs_from(
        &home,
        paths::env_path_dir("XDG_DATA_HOME"),
        paths::env_path_dir("PI_CODING_AGENT_SESSION_DIR"),
    );
    if session_dirs.is_empty() {
        debug!("OMP session directory not found, skipping OMP");
    }
    Ok(session_dirs)
}

fn resolve_omp_session_dirs_from(
    home: &Path,
    xdg_data_home: Option<PathBuf>,
    session_dir_override: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut session_dirs = Vec::new();
    let mut seen = HashSet::new();

    if let Some(session_dir) = session_dir_override {
        push_existing_unique_dir(&mut session_dirs, &mut seen, session_dir);
    }

    push_existing_unique_dir(
        &mut session_dirs,
        &mut seen,
        home.join(".omp").join("agent").join("sessions"),
    );

    let profiles = home.join(".omp").join("profiles");
    if let Ok(entries) = fs::read_dir(profiles) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                push_existing_unique_dir(
                    &mut session_dirs,
                    &mut seen,
                    path.join("agent").join("sessions"),
                );
            }
        }
    }

    let xdg_root = xdg_data_home.unwrap_or_else(|| home.join(".local").join("share"));
    push_existing_unique_dir(&mut session_dirs, &mut seen, xdg_root.join("omp").join("sessions"));
    let xdg_profiles = xdg_root.join("omp").join("profiles");
    if let Ok(entries) = fs::read_dir(xdg_profiles) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                push_existing_unique_dir(&mut session_dirs, &mut seen, path.join("sessions"));
            }
        }
    }

    session_dirs
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::adapters::file_scan::{self, FileScanEntry};
    use crate::adapters::pi_session::{EVENT_PARSER_VERSION, extract_session_id_from_filename};
    use crate::types::{FileEvidenceKind, FileOperation, Role};
    use serde_json::Value;

    fn parse_omp_session_file(
        entry: FileScanEntry,
        mtime_ms: i64,
        include_events: bool,
    ) -> anyhow::Result<Option<RawSession>> {
        pi_session::parse_session_file(entry, mtime_ms, include_events, Format::Omp)
    }

    fn collect_omp_entries(session_dirs: &[PathBuf]) -> Vec<FileScanEntry> {
        pi_session::collect_entries(session_dirs, Format::Omp)
    }

    fn decode_session_dir_name(name: &str) -> Option<String> {
        pi_session::decode_session_dir_name(name, Format::Omp)
    }

    fn temp_omp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "recall-omp-test-{}-{}",
            label,
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn title_slot_line(title: &str) -> String {
        let mut value = serde_json::json!({
            "type": "title",
            "v": 1,
            "title": title,
            "source": "auto",
            "updatedAt": "1970-01-01T00:00:01.000Z",
            "pad": ""
        });
        loop {
            let serialized = serde_json::to_string(&value).unwrap();
            if serialized.len() >= 255 {
                return serialized;
            }
            let pad = " ".repeat(255 - serialized.len());
            value["pad"] = Value::String(pad);
        }
    }

    fn write_omp_session(
        dir: &Path,
        session_id: &str,
        title: Option<&str>,
        lines: &[Value],
    ) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("2026-09-01T16-39-58-396Z_{session_id}.jsonl"));
        let mut file = fs::File::create(&path).unwrap();
        if let Some(title) = title {
            writeln!(file, "{}", title_slot_line(title)).unwrap();
        }
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        path
    }

    #[test]
    fn resume_uses_official_flag() {
        let command = OmpAdapter.resume_command("01a05dd7-7cbc-7005-818b-73de30e4dc42").unwrap();
        assert_eq!(command.program, "omp");
        assert_eq!(
            command.args,
            vec!["--resume".to_string(), "01a05dd7-7cbc-7005-818b-73de30e4dc42".to_string()]
        );
    }

    #[test]
    fn extract_session_id_from_filename_reads_uuid_tail() {
        assert_eq!(
            extract_session_id_from_filename(
                "2026-09-01T16-39-58-396Z_01a05dd7-7cbc-7005-818b-73de30e4dc42"
            ),
            Some("01a05dd7-7cbc-7005-818b-73de30e4dc42".to_string())
        );
        assert_eq!(extract_session_id_from_filename("not-a-session"), None);
    }

    #[test]
    fn decode_session_dir_name_reads_absolute_and_home_buckets() {
        assert_eq!(decode_session_dir_name("--private-tmp--").as_deref(), Some("/private/tmp"));
        assert_eq!(decode_session_dir_name("-git-samzong-Recall"), None);
    }

    #[test]
    fn resolve_session_dirs_finds_default_and_profile_roots() {
        let root = temp_omp_root("dirs");
        let default_sessions = root.join(".omp").join("agent").join("sessions");
        let profile_sessions =
            root.join(".omp").join("profiles").join("work").join("agent").join("sessions");
        fs::create_dir_all(&default_sessions).unwrap();
        fs::create_dir_all(&profile_sessions).unwrap();

        let dirs = resolve_omp_session_dirs_from(&root, None, None);
        assert!(dirs.iter().any(|dir| dir == &default_sessions));
        assert!(dirs.iter().any(|dir| dir == &profile_sessions));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_session_dirs_ignores_pi_and_custom_agent_roots() {
        let root = temp_omp_root("no-pi");
        let omp_sessions = root.join(".omp").join("agent").join("sessions");
        let pi_sessions = root.join(".pi").join("agent").join("sessions");
        let custom_sessions = root.join("custom-agent").join("sessions");
        fs::create_dir_all(&omp_sessions).unwrap();
        fs::create_dir_all(&pi_sessions).unwrap();
        fs::create_dir_all(&custom_sessions).unwrap();

        assert_eq!(resolve_omp_session_dirs_from(&root, None, None), vec![omp_sessions]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_session_dirs_skips_missing_roots() {
        let root = temp_omp_root("missing");
        assert!(resolve_omp_session_dirs_from(&root, None, None).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_session_dirs_includes_xdg_omp_sessions() {
        let root = temp_omp_root("xdg");
        let xdg = root.join("xdg-data");
        let xdg_sessions = xdg.join("omp").join("sessions");
        let profile_sessions = xdg.join("omp").join("profiles").join("work").join("sessions");
        fs::create_dir_all(&xdg_sessions).unwrap();
        fs::create_dir_all(&profile_sessions).unwrap();
        let dirs = resolve_omp_session_dirs_from(&root, Some(xdg), None);
        assert!(dirs.iter().any(|dir| dir == &xdg_sessions));
        assert!(dirs.iter().any(|dir| dir == &profile_sessions));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_omp_session_file_reads_title_slot_messages_and_usage() {
        let root = temp_omp_root("parse");
        let session_dir = root.join("-git-samzong-Recall");
        let session_id = "01a05dd7-7cbc-7005-818b-73de30e4dc42";
        let path = write_omp_session(
            &session_dir,
            session_id,
            Some("Add omp.sh as recall adapter"),
            &[
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": session_id,
                    "timestamp": "1970-01-01T00:00:01.000Z",
                    "cwd": "/Users/x/git/samzong/Recall"
                }),
                serde_json::json!({
                    "type": "model_change",
                    "id": "model1",
                    "parentId": null,
                    "timestamp": "1970-01-01T00:00:01.500Z",
                    "model": "openrouter/deepseek/deepseek-v4-flash-0731"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "user1",
                    "parentId": null,
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": "hello omp"}],
                        "timestamp": 2000
                    }
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "assistant1",
                    "parentId": "user1",
                    "timestamp": "1970-01-01T00:00:03.000Z",
                    "message": {
                        "role": "assistant",
                        "content": [
                            {"type": "thinking", "thinking": "hidden chain of thought"},
                            {"type": "toolCall", "id": "read-call", "name": "read", "arguments": {"path": "README.md"}},
                            {"type": "text", "text": "done"},
                            {"type": "image", "mimeType": "image/png"}
                        ],
                        "provider": "openrouter",
                        "model": "deepseek/deepseek-v4-flash-0731",
                        "usage": {
                            "input": 10,
                            "output": 3,
                            "cacheRead": 2,
                            "cacheWrite": 1,
                            "reasoningTokens": 4,
                            "totalTokens": 20
                        },
                        "timestamp": 3000
                    }
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "tool1",
                    "parentId": "assistant1",
                    "timestamp": "1970-01-01T00:00:04.000Z",
                    "message": {
                        "role": "toolResult",
                        "toolName": "read",
                        "toolCallId": "read-call",
                        "isError": false,
                        "content": [{"type": "text", "text": "file content"}],
                        "timestamp": 4000
                    }
                }),
            ],
        );
        let slot = fs::read_to_string(&path).unwrap();
        assert_eq!(slot.find('\n'), Some(255));

        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let raw = parse_omp_session_file(
            FileScanEntry {
                session_id: session_id.to_string(),
                stat_target: path.clone(),
                directory: Some("/wrong".to_string()),
            },
            mtime,
            true,
        )
        .unwrap()
        .unwrap();

        assert_eq!(raw.source_id, session_id);
        assert_eq!(raw.directory.as_deref(), Some("/Users/x/git/samzong/Recall"));
        assert_eq!(raw.custom_title.as_deref(), Some("Add omp.sh as recall adapter"));
        assert_eq!(raw.started_at, 1_000);
        assert_eq!(raw.updated_at, Some(mtime));
        assert_eq!(raw.source_file_path.as_deref(), path.to_str());
        assert_eq!(raw.messages.len(), 2);
        assert_eq!(raw.messages[0].role, Role::User);
        assert_eq!(raw.messages[0].content, "hello omp");
        assert!(raw.messages[1].content.contains("done"));
        assert_eq!(raw.messages[1].content, "done");
        assert!(!raw.messages[1].content.contains("hidden chain of thought"));
        assert!(!raw.messages[1].content.contains("image/png"));

        assert_eq!(raw.events.len(), 2);
        let call = &raw.events[0];
        assert_eq!(call.tool_call_id.as_deref(), Some("read-call"));
        assert!(call.source_event_id.as_deref().unwrap().starts_with("assistant1:line:"));
        assert_eq!(call.source_path.as_deref(), path.to_str());
        assert_eq!(call.timestamp, Some(3_000));
        assert_eq!(call.message_seq, Some(0));
        assert_eq!(call.files.len(), 1);
        assert_eq!(call.files[0].path, "README.md");
        assert_eq!(call.files[0].operation, FileOperation::Read);
        assert_eq!(call.files[0].kind, FileEvidenceKind::Call);
        assert_eq!(call.files[0].cwd, raw.directory);
        let result = &raw.events[1];
        assert_eq!(result.tool_call_id, call.tool_call_id);
        assert_eq!(result.message_seq, Some(1));
        assert_eq!(result.status.as_deref(), Some("success"));
        assert!(result.files.is_empty());
        let payload: Value = serde_json::from_str(result.attrs_json.as_deref().unwrap()).unwrap();
        assert_eq!(payload.pointer("/message/content/0/text"), Some(&Value::from("file content")));
        assert_eq!(raw.event_parser_version, Some(EVENT_PARSER_VERSION));

        assert_eq!(raw.usage_events.len(), 1);
        let event = &raw.usage_events[0];
        assert_eq!(event.event_key, "message:assistant1");
        assert_eq!(event.message_seq, Some(1));
        assert_eq!(event.timestamp, 3_000);
        assert_eq!(event.provider, "openrouter");
        assert_eq!(event.model, "deepseek/deepseek-v4-flash-0731");
        assert_eq!(event.input_tokens, 10);
        assert_eq!(event.output_tokens, 3);
        assert_eq!(event.cache_read_tokens, 2);
        assert_eq!(event.cache_write_tokens, 1);
        assert_eq!(event.reasoning_tokens, 4);
        assert_eq!(event.token_source, crate::types::TokenSource::Observed);
        assert_eq!(event.parser_version, USAGE_PARSER_VERSION);
        assert_eq!(event.source_path.as_deref(), Some(path.to_string_lossy().as_ref()));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn usage_normalizes_reasoning_included_in_output() {
        pi_session::test_support::usage_normalizes_reasoning_included_in_output(Format::Omp);
    }

    #[test]
    fn parse_omp_session_file_prefers_title_slot_over_header() {
        let root = temp_omp_root("title-pref");
        let session_dir = root.join("--tmp-omp-project--");
        let session_id = "01a05dd7-7cbc-7005-818b-73de30e4dc42";
        let path = write_omp_session(
            &session_dir,
            session_id,
            Some("slot title"),
            &[
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": session_id,
                    "title": "header title",
                    "timestamp": "1970-01-01T00:00:01.000Z",
                    "cwd": "/tmp/omp-project"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "user1",
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "message": {
                        "role": "user",
                        "content": "hello",
                        "timestamp": 2000
                    }
                }),
            ],
        );
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let raw = parse_omp_session_file(
            FileScanEntry {
                session_id: session_id.to_string(),
                stat_target: path,
                directory: None,
            },
            mtime,
            true,
        )
        .unwrap()
        .unwrap();

        assert_eq!(raw.custom_title.as_deref(), Some("slot title"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_omp_entries_ignores_sidecar_logs() {
        let root = temp_omp_root("sidecar");
        let session_dir = root.join("-git-samzong-Recall");
        let session_id = "01a05dd7-7cbc-7005-818b-73de30e4dc42";
        let path = write_omp_session(
            &session_dir,
            session_id,
            None,
            &[serde_json::json!({
                "type": "session",
                "version": 3,
                "id": session_id,
                "timestamp": "1970-01-01T00:00:01.000Z",
                "cwd": "/tmp/omp-project"
            })],
        );
        let sidecar = session_dir.join(path.file_stem().unwrap());
        fs::create_dir_all(&sidecar).unwrap();
        fs::write(sidecar.join("4.bash-original.log"), "log").unwrap();

        let entries = collect_omp_entries(&[session_dir]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, session_id);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_omp_session_file_skips_hidden_bash_execution() {
        pi_session::test_support::skips_hidden_bash_execution(Format::Omp);
    }

    #[test]
    fn parse_omp_session_file_skips_fork_inherited_usage() {
        pi_session::test_support::skips_fork_inherited_usage(Format::Omp);
    }

    #[test]
    fn scan_for_sync_skips_unchanged_session_when_usage_state_is_current() {
        pi_session::test_support::scan_for_sync_skips_unchanged_session_when_usage_state_is_current(
            Format::Omp,
        );
    }

    #[test]
    fn parse_omp_session_maps_parent_session_to_primary_fork() {
        pi_session::test_support::maps_parent_session_to_primary_fork(Format::Omp);
    }

    #[test]
    fn parse_omp_session_drops_unresolvable_parent_session() {
        pi_session::test_support::drops_unresolvable_parent_session(Format::Omp);
    }
}
