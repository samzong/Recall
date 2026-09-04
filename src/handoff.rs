use anyhow::Result;

use crate::adapters::{self, ResumeCommand};
use crate::types::{Message, Role, Session};
use crate::utils::binary_on_path;

const LAST_USER_CHARS: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HandoffTarget {
    pub(crate) id: String,
    pub(crate) label: String,
}

pub(crate) fn available_targets() -> Vec<HandoffTarget> {
    available_targets_with(binary_on_path)
}

pub(crate) fn available_targets_with(present: impl Fn(&str) -> bool) -> Vec<HandoffTarget> {
    let mut targets = Vec::new();
    for adapter in adapters::all_adapters() {
        let Some(command) = adapter.start_command(String::new()) else {
            continue;
        };
        if present(&command.program) {
            targets.push(target_from_adapter(adapter.as_ref()));
        }
    }
    targets
}

pub(crate) fn build_prompt(session: &Session, messages: &[Message]) -> String {
    let last_user = messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(|message| truncate_chars(&message.content, LAST_USER_CHARS))
        .filter(|content| !content.is_empty())
        .unwrap_or_else(|| "(none)".to_string());

    let mut prompt = String::new();
    prompt.push_str("Use Recall session ");
    prompt.push_str(&session.id);
    prompt.push_str(" as prior context. This is a handoff, not a native resume.\n");
    prompt.push_str("Title: ");
    prompt.push_str(&session.title);
    prompt.push('\n');
    prompt.push_str("Source: ");
    prompt.push_str(&session.source);
    prompt.push_str(" (");
    prompt.push_str(&session.source_id);
    prompt.push_str(")\n");
    if let Some(directory) = session.directory.as_deref() {
        prompt.push_str("Directory: ");
        prompt.push_str(directory);
        prompt.push('\n');
    }
    prompt.push_str("Messages: ");
    prompt.push_str(&messages.len().to_string());
    prompt.push('\n');
    prompt.push_str("Last user request:\n");
    prompt.push_str(&last_user);
    prompt.push_str("\n\nLoad evidence with Recall MCP get_session or `recall session show --id ");
    prompt.push_str(&session.id);
    prompt.push_str("`.\n");
    prompt
}

pub(crate) fn command_for_target(target: &HandoffTarget, prompt: String) -> Result<ResumeCommand> {
    adapters::all_adapters()
        .into_iter()
        .find(|adapter| adapter.id() == target.id)
        .and_then(|adapter| adapter.start_command(prompt))
        .ok_or_else(|| anyhow::anyhow!("unsupported handoff target: {}", target.id))
}

pub(crate) fn target_for(target_id: &str) -> Result<HandoffTarget> {
    let id = target_id.to_ascii_lowercase();
    let supported = supported_targets();
    supported.into_iter().find(|target| target.id == id).ok_or_else(|| {
        let names = supported_target_ids().join(", ");
        anyhow::anyhow!("unsupported handoff target: {target_id} (supported: {names})")
    })
}

pub(crate) fn require_installed(command: &ResumeCommand) -> Result<()> {
    if binary_on_path(&command.program) {
        return Ok(());
    }
    anyhow::bail!("{} not found on PATH", command.program)
}

fn supported_targets() -> Vec<HandoffTarget> {
    adapters::all_adapters()
        .into_iter()
        .filter(|adapter| adapter.start_command(String::new()).is_some())
        .map(|adapter| target_from_adapter(adapter.as_ref()))
        .collect()
}

fn supported_target_ids() -> Vec<String> {
    supported_targets().into_iter().map(|target| target.id).collect()
}

fn target_from_adapter(adapter: &dyn adapters::SourceAdapter) -> HandoffTarget {
    HandoffTarget { id: adapter.id().to_string(), label: display_label(adapter.id()) }
}

fn display_label(id: &str) -> String {
    id.split('-')
        .map(|part| {
            if part.eq_ignore_ascii_case("cli") {
                "CLI".to_string()
            } else {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, Role, Session};

    fn session() -> Session {
        Session {
            id: "s1".to_string(),
            source: "grok".to_string(),
            source_id: "raw1".to_string(),
            title: "Fix login bug".to_string(),
            directory: Some("/tmp/project".to_string()),
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
            started_at: 0,
            updated_at: None,
            message_count: 1,
            entrypoint: None,
            custom_title: None,
            summary: None,
            duration_minutes: None,
            source_file_path: None,
            is_import: true,
        }
    }

    fn message(role: Role, content: &str, seq: u32) -> Message {
        Message {
            session_id: "s1".to_string(),
            role,
            content: content.to_string(),
            timestamp: None,
            seq,
        }
    }

    fn target(id: &str) -> HandoffTarget {
        target_for(id).unwrap()
    }

    #[test]
    fn handoff_prompt_is_a_recall_pointer() {
        let prompt = build_prompt(
            &session(),
            &[
                message(Role::User, "old request that must not leak", 0),
                message(Role::Assistant, "old answer", 1),
                message(Role::User, "continue this work", 2),
            ],
        );

        assert!(prompt.contains("This is a handoff, not a native resume."));
        assert!(prompt.contains("Use Recall session s1"));
        assert!(prompt.contains("Title: Fix login bug"));
        assert!(prompt.contains("Source: grok (raw1)"));
        assert!(prompt.contains("Directory: /tmp/project"));
        assert!(prompt.contains("continue this work"));
        assert!(prompt.contains("recall session show --id s1"));
        assert!(!prompt.contains("old request that must not leak"));
        assert!(!prompt.contains("old answer"));
        assert!(!prompt.contains("## User"));
    }

    #[test]
    fn handoff_commands_use_adapter_start() {
        let cases = [
            ("codex", "codex", vec!["prompt"]),
            ("grok", "grok", vec!["prompt"]),
            ("claude-code", "claude", vec!["prompt"]),
            ("opencode", "opencode", vec!["run", "-i", "prompt"]),
            ("antigravity-cli", "agy", vec!["-i", "prompt"]),
            ("cursor", "agent", vec!["prompt"]),
            ("kilo-code", "kilo", vec!["--prompt", "prompt"]),
            ("mimo-code", "mimo", vec!["--prompt", "prompt"]),
        ];

        for (target_id, program, args) in cases {
            let command = command_for_target(&target(target_id), "prompt".to_string()).unwrap();
            assert_eq!(command.program, program);
            assert_eq!(command.args, args);
        }
    }

    #[test]
    fn available_targets_keep_installed_startable_adapters() {
        let targets = available_targets_with(|program| matches!(program, "codex" | "agy"));
        let ids: Vec<_> = targets.iter().map(|target| target.id.as_str()).collect();
        assert_eq!(ids, vec!["codex", "antigravity-cli"]);
        assert_eq!(targets[0].label, "Codex");
        assert_eq!(targets[1].label, "Antigravity CLI");
    }

    #[test]
    fn target_for_rejects_unknown_and_unstartable_ids() {
        let error = target_for("not-a-tool").unwrap_err().to_string();
        assert!(error.contains("unsupported handoff target: not-a-tool"));
        assert!(error.contains("codex"));
        assert!(!error.contains("kimi-code"));
        assert!(target_for("CODEX").is_ok());
    }
}
