use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use tracing::debug;

use crate::adapters::pi_session::{self, Format, USAGE_PARSER_VERSION, push_existing_unique_dir};
use crate::adapters::{
    AdapterSyncContext, RawSession, ResumeCommand, SourceAdapter, SyncScanResult,
};
use serde_json::Value;

pub(crate) struct PiAdapter;

impl SourceAdapter for PiAdapter {
    fn id(&self) -> &str {
        "pi"
    }

    fn label(&self) -> &str {
        "PI"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand::new("pi", &["--session", source_id]))
    }

    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        Some(crate::adapters::prompt_start("pi", prompt))
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        pi_session::scan(&resolve_pi_session_dirs()?, Format::Pi)
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        Ok(Some(pi_session::scan_for_sync(
            &resolve_pi_session_dirs()?,
            context,
            since_ts,
            include_events,
            Format::Pi,
        )?))
    }
}

fn resolve_pi_session_dirs() -> anyhow::Result<Vec<PathBuf>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let mut session_dirs = Vec::new();
    let mut seen = HashSet::new();

    let env_session_dir =
        std::env::var("PI_CODING_AGENT_SESSION_DIR").ok().filter(|path| !path.trim().is_empty());
    let agent_dir = std::env::var("PI_CODING_AGENT_DIR")
        .ok()
        .filter(|path| !path.trim().is_empty())
        .map(|path| expand_home_path(path.trim(), &home))
        .unwrap_or_else(|| home.join(".pi").join("agent"));

    if let Some(session_dir) = env_session_dir.as_deref() {
        push_existing_unique_dir(
            &mut session_dirs,
            &mut seen,
            expand_home_path(session_dir.trim(), &home),
        );
        if session_dirs.is_empty() {
            debug!("Pi session directory from PI_CODING_AGENT_SESSION_DIR not found, skipping Pi");
            return Ok(session_dirs);
        }
    } else if let Some(session_dir) = settings_session_dir(&agent_dir, &home) {
        push_existing_unique_dir(&mut session_dirs, &mut seen, session_dir);
    }
    if env_session_dir.is_none() {
        push_existing_unique_dir(&mut session_dirs, &mut seen, agent_dir.join("sessions"));
    }

    if session_dirs.is_empty() {
        debug!("Pi session directory not found, skipping Pi");
    }

    Ok(session_dirs)
}

fn settings_session_dir(agent_dir: &Path, home: &Path) -> Option<PathBuf> {
    settings_session_dir_with_cwd(agent_dir, home, std::env::current_dir().ok().as_deref())
}

fn settings_session_dir_with_cwd(
    agent_dir: &Path,
    home: &Path,
    current_dir: Option<&Path>,
) -> Option<PathBuf> {
    let global = session_dir_from_settings(&agent_dir.join("settings.json"), home);
    let Some(current_dir) = current_dir else {
        return global;
    };
    let project_settings_dir = current_dir.join(".pi");
    session_dir_from_settings(&project_settings_dir.join("settings.json"), home).or(global)
}

fn session_dir_from_settings(settings_path: &Path, home: &Path) -> Option<PathBuf> {
    let content = fs::read_to_string(settings_path).ok()?;
    let settings: Value = serde_json::from_str(&content).ok()?;
    let session_dir = settings.get("sessionDir")?.as_str()?.trim();
    if session_dir.is_empty() {
        return None;
    }
    let session_dir = expand_home_path(session_dir, home);
    if session_dir.is_relative() {
        return Some(settings_path.parent()?.join(session_dir));
    }
    Some(session_dir)
}

fn expand_home_path(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::adapters::file_scan::{self, FileScanEntry};
    use crate::adapters::pi_session::{
        EVENT_PARSER_VERSION, extract_session_id_from_filename,
        extract_usage_event as extract_pi_usage_event,
    };
    use crate::types::{FileEvidenceKind, FileOperation, Role};
    use serde_json::Value;

    fn parse_pi_session_file(
        entry: FileScanEntry,
        mtime_ms: i64,
        include_events: bool,
    ) -> anyhow::Result<Option<RawSession>> {
        pi_session::parse_session_file(entry, mtime_ms, include_events, Format::Pi)
    }

    fn temp_pi_root(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("recall-pi-test-{}-{}", label, uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_pi_session(dir: &Path, session_id: &str, lines: &[Value]) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("2026-05-24T17-04-51-496Z_{session_id}.jsonl"));
        let mut file = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        path
    }

    #[test]
    fn extract_session_id_from_filename_reads_pi_uuid_tail() {
        assert_eq!(
            extract_session_id_from_filename(
                "2026-05-24T17-04-51-496Z_019e5af2-5528-7d10-888a-b299c21d0e2e"
            ),
            Some("019e5af2-5528-7d10-888a-b299c21d0e2e".to_string())
        );
        assert_eq!(extract_session_id_from_filename("not-a-session"), None);
    }

    #[test]
    fn session_dir_from_settings_resolves_relative_paths_from_settings_scope() {
        let root = temp_pi_root("settings-dir");
        let home = root.join("home");
        let agent_dir = home.join(".pi").join("agent");
        fs::create_dir_all(&agent_dir).unwrap();
        let settings_path = agent_dir.join("settings.json");
        fs::write(&settings_path, r#"{"sessionDir":"custom-sessions"}"#).unwrap();

        assert_eq!(
            session_dir_from_settings(&settings_path, &home).as_deref(),
            Some(agent_dir.join("custom-sessions").as_path())
        );

        fs::write(&settings_path, r#"{"sessionDir":"~/pi-sessions"}"#).unwrap();
        assert_eq!(
            session_dir_from_settings(&settings_path, &home).as_deref(),
            Some(home.join("pi-sessions").as_path())
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn settings_session_dir_keeps_global_when_current_dir_is_unavailable() {
        let root = temp_pi_root("settings-no-cwd");
        let home = root.join("home");
        let agent_dir = home.join(".pi").join("agent");
        let global_session_dir = root.join("global-sessions");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(&global_session_dir).unwrap();
        fs::write(
            agent_dir.join("settings.json"),
            format!(r#"{{"sessionDir":"{}"}}"#, global_session_dir.display()),
        )
        .unwrap();

        assert_eq!(
            settings_session_dir_with_cwd(&agent_dir, &home, None).as_deref(),
            Some(global_session_dir.as_path())
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_pi_session_file_extracts_messages_and_usage() {
        let root = temp_pi_root("parse");
        let session_dir = root.join("--tmp-pi-project--");
        let session_id = "019e5af2-5528-7d10-888a-b299c21d0e2e";
        let path = write_pi_session(
            &session_dir,
            session_id,
            &[
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": session_id,
                    "timestamp": "1970-01-01T00:00:01.000Z",
                    "cwd": "/tmp/pi-project"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "user1",
                    "parentId": null,
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": "hello pi"}],
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
                            {"type": "toolCall", "id": "edit-call", "name": "edit", "arguments": {"path": " spaced.rs ", "edits": [{"oldText": "old", "newText": "new"}]}},
                            {"type": "toolCall", "id": "write-call", "name": "write", "arguments": {"path": "new.rs", "content": "new file"}},
                            {"type": "image", "mimeType": "image/png"}
                        ],
                        "provider": "openai-codex",
                        "model": "gpt-5.5",
                        "usage": {
                            "input": 10,
                            "output": 3,
                            "cacheRead": 2,
                            "cacheWrite": 1,
                            "totalTokens": 16,
                            "cost": {"total": 0.1}
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
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let raw = parse_pi_session_file(
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
        assert_eq!(raw.directory.as_deref(), Some("/tmp/pi-project"));
        assert_eq!(raw.started_at, 1_000);
        assert_eq!(raw.updated_at, Some(mtime));
        assert_eq!(raw.source_file_path.as_deref(), path.to_str());
        assert_eq!(raw.messages.len(), 1);
        assert_eq!(raw.messages[0].role, Role::User);
        assert_eq!(raw.messages[0].content, "hello pi");
        assert_eq!(raw.events.len(), 4);
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
        assert_eq!(raw.events[1].files[0].path, " spaced.rs ");
        assert_eq!(raw.events[1].files[0].operation, FileOperation::Write);
        assert_eq!(raw.events[2].files[0].operation, FileOperation::Write);
        let payload: Value =
            serde_json::from_str(raw.events[1].attrs_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            payload.pointer("/message/content/2/arguments/edits/0/oldText"),
            Some(&Value::from("old"))
        );
        let result = &raw.events[3];
        assert_eq!(result.tool_call_id, call.tool_call_id);
        assert_eq!(result.message_seq, Some(0));
        assert_eq!(result.status.as_deref(), Some("success"));
        assert!(result.files.is_empty());
        let payload: Value = serde_json::from_str(result.attrs_json.as_deref().unwrap()).unwrap();
        assert_eq!(payload.pointer("/message/content/0/text"), Some(&Value::from("file content")));
        assert_eq!(raw.event_parser_version, Some(EVENT_PARSER_VERSION));

        assert_eq!(raw.usage_events.len(), 1);
        let event = &raw.usage_events[0];
        assert_eq!(event.event_key, "message:assistant1");
        assert_eq!(event.message_seq, None);
        assert_eq!(event.timestamp, 3_000);
        assert_eq!(event.provider, "openai-codex");
        assert_eq!(event.model, "gpt-5.5");
        assert_eq!(event.input_tokens, 10);
        assert_eq!(event.output_tokens, 3);
        assert_eq!(event.cache_read_tokens, 2);
        assert_eq!(event.cache_write_tokens, 1);
        assert_eq!(event.token_source, crate::types::TokenSource::Observed);
        assert_eq!(event.parser_version, USAGE_PARSER_VERSION);
        assert_eq!(event.source_path.as_deref(), Some(path.to_string_lossy().as_ref()));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn usage_normalizes_reasoning_included_in_output() {
        pi_session::test_support::usage_normalizes_reasoning_included_in_output(Format::Pi);
    }

    #[test]
    fn usage_caps_reasoning_that_exceeds_inclusive_output() {
        let entry = serde_json::json!({"id": "assistant1"});
        let message = serde_json::json!({
            "provider": "xai",
            "model": "grok",
            "usage": {
                "input": 10,
                "output": 3,
                "cacheRead": 2,
                "reasoningTokens": 4,
                "totalTokens": 15
            }
        });

        let event = extract_pi_usage_event(
            &entry,
            &message,
            1,
            3_000,
            Some(1),
            (None, None),
            "/tmp/session.jsonl",
        )
        .unwrap();

        assert_eq!(event.output_tokens, 0);
        assert_eq!(event.reasoning_tokens, 3);
    }

    #[test]
    fn parse_pi_session_file_indexes_custom_role_message_content() {
        let root = temp_pi_root("custom-role");
        let session_dir = root.join("--tmp-pi-project--");
        let session_id = "019e5af2-5528-7d10-888a-b299c21d0e2e";
        let path = write_pi_session(
            &session_dir,
            session_id,
            &[
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": session_id,
                    "timestamp": "1970-01-01T00:00:01.000Z",
                    "cwd": "/tmp/pi-project"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "custom1",
                    "parentId": null,
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "message": {
                        "role": "custom",
                        "customType": "extension-context",
                        "content": [{"type": "text", "text": "injected context"}],
                        "display": false,
                        "timestamp": 2000
                    }
                }),
            ],
        );
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let raw = parse_pi_session_file(
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

        assert_eq!(raw.messages.len(), 1);
        assert_eq!(raw.messages[0].role, Role::User);
        assert_eq!(raw.messages[0].content, "injected context");
        assert_eq!(raw.messages[0].timestamp, Some(2_000));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_pi_session_file_skips_hidden_bash_execution() {
        pi_session::test_support::skips_hidden_bash_execution(Format::Pi);
    }

    #[test]
    fn parse_pi_session_file_uses_model_change_for_usage_only_assistant_message() {
        let root = temp_pi_root("usage-only");
        let session_dir = root.join("--tmp-pi-project--");
        let session_id = "019e5af2-5528-7d10-888a-b299c21d0e2e";
        let path = write_pi_session(
            &session_dir,
            session_id,
            &[
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": session_id,
                    "timestamp": "1970-01-01T00:00:01.000Z",
                    "cwd": "/tmp/pi-project"
                }),
                serde_json::json!({
                    "type": "model_change",
                    "id": "model1",
                    "parentId": null,
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "provider": "anthropic",
                    "modelId": "claude-opus-4-7"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "assistant-empty",
                    "parentId": "model1",
                    "timestamp": "1970-01-01T00:00:03.000Z",
                    "message": {
                        "role": "assistant",
                        "content": [],
                        "usage": {
                            "input": 5,
                            "output": 7,
                            "cacheRead": 11,
                            "cacheWrite": 13,
                            "totalTokens": 36
                        },
                        "timestamp": 3000
                    }
                }),
            ],
        );
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let raw = parse_pi_session_file(
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

        assert!(raw.messages.is_empty());
        assert_eq!(raw.started_at, 1_000);
        assert_eq!(raw.usage_events.len(), 1);
        assert_eq!(raw.usage_events[0].message_seq, None);
        assert_eq!(raw.usage_events[0].provider, "anthropic");
        assert_eq!(raw.usage_events[0].model, "claude-opus-4-7");
        assert_eq!(raw.usage_events[0].input_tokens, 5);
        assert_eq!(raw.usage_events[0].output_tokens, 7);
        assert_eq!(raw.usage_events[0].cache_read_tokens, 11);
        assert_eq!(raw.usage_events[0].cache_write_tokens, 13);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_pi_session_file_skips_fork_inherited_usage() {
        pi_session::test_support::skips_fork_inherited_usage(Format::Pi);
    }

    #[test]
    fn scan_for_sync_skips_unchanged_session_when_usage_state_is_current() {
        pi_session::test_support::scan_for_sync_skips_unchanged_session_when_usage_state_is_current(
            Format::Pi,
        );
    }

    #[test]
    fn parse_pi_session_maps_parent_session_to_primary_fork() {
        pi_session::test_support::maps_parent_session_to_primary_fork(Format::Pi);
    }

    #[test]
    fn parse_pi_session_drops_unresolvable_parent_session() {
        pi_session::test_support::drops_unresolvable_parent_session(Format::Pi);
    }
}
