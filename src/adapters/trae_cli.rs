use std::path::PathBuf;

use tracing::debug;

use crate::adapters::AdapterSyncContext;
use crate::adapters::codex::{self, RolloutDialect};
use crate::adapters::paths::{self, resolve_home_dir};
use crate::adapters::{RawSession, ResumeCommand, SourceAdapter, SyncScanResult};

pub(crate) struct TraeCliAdapter;

impl SourceAdapter for TraeCliAdapter {
    fn id(&self) -> &str {
        "trae-cli"
    }

    fn label(&self) -> &str {
        "TR"
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(codex::USAGE_PARSER_VERSION)
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand::new("traecli", &["resume", source_id]))
    }

    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        Some(crate::adapters::prompt_start("traecli", prompt))
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(root) = resolve_cli_home()? else {
            return Ok(vec![]);
        };
        codex::scan_rollouts(&root, RolloutDialect::Trae)
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some(root) = resolve_cli_home()? else {
            return Ok(Some(SyncScanResult::default()));
        };
        Ok(Some(codex::scan_rollouts_for_sync(
            &root,
            RolloutDialect::Trae,
            context,
            since_ts,
            include_events,
        )?))
    }
}

fn resolve_cli_home() -> anyhow::Result<Option<PathBuf>> {
    if let Some(dir) = paths::env_path_dir("TRAECLI_HOME") {
        if dir.is_dir() {
            return Ok(Some(dir));
        }
        debug!("TRAECLI_HOME not found, skipping TRAE CLI");
        return Ok(None);
    }
    if let Some(dir) = paths::env_path_dir("TRAE_HOME") {
        let dir = dir.join("cli");
        if dir.is_dir() {
            return Ok(Some(dir));
        }
        debug!("TRAE_HOME has no cli directory, skipping TRAE CLI");
        return Ok(None);
    }
    resolve_home_dir(".trae/cli", "~/.trae/cli not found, skipping TRAE CLI")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use serde_json::{Value, json};

    use crate::adapters::codex::{RolloutDialect, parse_rollout};

    const SESSION: &str = "019fbcca-0000-7000-8000-000000000001";

    fn line(at: &str, kind: &str, payload: Value) -> Value {
        json!({"timestamp": at, "type": kind, "payload": payload})
    }

    fn write_rollout(dir: &Path, lines: &[Value]) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("rollout-2026-10-06T09-00-00-{SESSION}.jsonl"));
        let meta = line(
            "2026-10-06T09:00:00.000Z",
            "session_meta",
            json!({"id": SESSION, "timestamp": "2026-10-06T09:00:00.000Z", "cwd": "/w/api", "thread_source": "user", "model_provider": "trae"}),
        );
        let body: String =
            std::iter::once(&meta).chain(lines).map(|line| format!("{line}\n")).collect();
        fs::write(&path, body).unwrap();
        path
    }

    fn conversation(path: &Path, dialect: RolloutDialect) -> Vec<(String, String)> {
        parse_rollout(path, true, dialect)
            .unwrap()
            .unwrap()
            .messages
            .iter()
            .map(|m| (m.role.as_str().to_string(), m.content.clone()))
            .collect()
    }

    #[test]
    fn keeps_confirmed_prompts_and_drops_runtime_injections() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_rollout(
            dir.path(),
            &[
                line(
                    "2026-10-06T09:00:01.000Z",
                    "response_item",
                    json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Warning: process limit notice"}]}),
                ),
                line(
                    "2026-10-06T09:00:01.100Z",
                    "event_msg",
                    json!({"type": "item_completed", "turn_id": "turn-1", "item": {"type": "UserMessage", "id": "u1", "content": [{"type": "text", "text": "rename the "}, {"type": "text", "text": "config loader"}]}}),
                ),
                line(
                    "2026-10-06T09:00:01.101Z",
                    "event_msg",
                    json!({"type": "user_message", "turn_id": "turn-1", "message": "rename the config loader"}),
                ),
                line(
                    "2026-10-06T09:00:02.000Z",
                    "event_msg",
                    json!({"type": "item_completed", "turn_id": "turn-1", "item": {"type": "AgentMessage", "id": "a1", "content": [{"type": "Text", "text": "Renamed it to LoadConfig."}]}}),
                ),
                line(
                    "2026-10-06T09:00:02.010Z",
                    "event_msg",
                    json!({"type": "agent_message", "message": "Renamed it to LoadConfig."}),
                ),
            ],
        );

        let expected = [
            ("user".to_string(), "rename the config loader".to_string()),
            ("assistant".to_string(), "Renamed it to LoadConfig.".to_string()),
        ];
        assert_eq!(conversation(&path, RolloutDialect::Trae), expected);
        assert!(
            conversation(&path, RolloutDialect::Codex)
                .iter()
                .any(|(_, text)| text.contains("process limit notice")),
            "control: the Codex dialect still trusts user response items"
        );
    }

    #[test]
    fn reads_tools_and_replies_once_across_streams() {
        let dir = tempfile::tempdir().unwrap();
        let exec_call = json!({"type": "function_call", "call_id": "call-1", "name": "exec",
            "arguments": "{\"command\":[\"bash\",\"-lc\",\"mv stale.go old.go\"],\"workdir\":\"/w/api/internal\"}"});
        let first_reply = json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Renaming the stale file."}]});
        let last_reply = json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Done."}]});
        let path = write_rollout(
            dir.path(),
            &[
                line(
                    "2026-10-06T09:00:00.500Z",
                    "event_msg",
                    json!({"type": "user_message", "turn_id": "turn-1", "message": "clean up retry"}),
                ),
                line("2026-10-06T09:00:00.600Z", "response_item", first_reply.clone()),
                line("2026-10-06T09:00:00.700Z", "response_item", exec_call.clone()),
                line("2026-10-06T09:00:00.800Z", "response_item", last_reply.clone()),
                line(
                    "2026-10-06T09:00:01.000Z",
                    "history_mutation",
                    json!({"turn_id": "turn-1", "operation": "append", "display_completions": [
                        {"turn_id": "turn-1", "item": {"type": "UserMessage", "id": "u1", "content": [{"type": "text", "text": "clean up retry"}]}}
                    ], "items": [
                        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "<environment_context>cwd</environment_context>"}]},
                        first_reply,
                        exec_call,
                        {"type": "function_call_output", "call_id": "call-1", "output": [{"type": "input_text", "text": "removed\n"}, {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}]},
                        last_reply,
                    ]}),
                ),
                line(
                    "2026-10-06T09:00:01.100Z",
                    "event_msg",
                    json!({"type": "item_completed", "turn_id": "turn-1", "item": {"type": "FileChange", "id": "fc-1", "status": "completed",
                        "changes": {"/w/api/internal/stale.go": {"type": "delete"}}}}),
                ),
                line(
                    "2026-10-06T09:00:01.200Z",
                    "history_mutation",
                    json!({"turn_id": "turn-1", "operation": "replace", "items": [
                        {"type": "function_call", "call_id": "call-9", "name": "exec", "arguments": "{\"command\":[\"bash\",\"-lc\",\"go vet ./...\"]}"}
                    ]}),
                ),
            ],
        );

        let session = parse_rollout(&path, true, RolloutDialect::Trae).unwrap().unwrap();

        let messages: Vec<_> =
            session.messages.iter().map(|m| (m.role.as_str(), m.content.as_str())).collect();
        assert_eq!(
            messages,
            [
                ("user", "clean up retry"),
                ("assistant", "Renaming the stale file."),
                ("assistant", "Done.")
            ]
        );
        let commands: Vec<_> =
            session.events.iter().filter(|event| event.kind == "command").collect();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].target.as_deref(), Some("mv stale.go old.go"));
        assert!(
            commands[0].files.iter().any(|file| file.cwd.as_deref() == Some("/w/api/internal")),
            "{:?}",
            commands[0].files
        );
        let output = session.events.iter().find(|event| event.kind == "tool_result").unwrap();
        assert_eq!(output.summary.as_deref(), Some("removed\n"));
        assert!(session.events.iter().any(|event| event.kind == "file_change"));
    }
}
