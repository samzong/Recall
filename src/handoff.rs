use anyhow::Result;

use crate::adapters::ResumeCommand;
use crate::transcript;
use crate::types::{Message, Session};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandoffTarget {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
}

pub(crate) const TARGETS: [HandoffTarget; 4] = [
    HandoffTarget { id: "codex", label: "Codex" },
    HandoffTarget { id: "grok", label: "Grok" },
    HandoffTarget { id: "claude-code", label: "Claude Code" },
    HandoffTarget { id: "opencode", label: "OpenCode" },
];

pub fn build_prompt(session: &Session, messages: &[Message]) -> String {
    format!(
        "Use this Recall indexed session transcript as context for a new session. This is a handoff, not a native resume.\n\n{}",
        transcript::render_plain(session, messages)
    )
}

pub fn command_for_target(target: &HandoffTarget, prompt: String) -> ResumeCommand {
    match target.id {
        "codex" => ResumeCommand { program: "codex".to_string(), args: vec![prompt] },
        "grok" => ResumeCommand { program: "grok".to_string(), args: vec![prompt] },
        "claude-code" => ResumeCommand { program: "claude".to_string(), args: vec![prompt] },
        "opencode" => ResumeCommand {
            program: "opencode".to_string(),
            args: vec!["run".to_string(), "-i".to_string(), prompt],
        },
        _ => unreachable!(),
    }
}

pub fn target_for(target_id: &str) -> Result<&'static HandoffTarget> {
    let id = target_id.to_ascii_lowercase();
    TARGETS.iter().find(|target| target.id == id).ok_or_else(|| {
        let supported = TARGETS.iter().map(|target| target.id).collect::<Vec<_>>().join(", ");
        anyhow::anyhow!("unsupported handoff target: {target_id} (supported: {supported})")
    })
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

    fn message() -> Message {
        Message {
            session_id: "s1".to_string(),
            role: Role::User,
            content: "continue this work".to_string(),
            timestamp: None,
            seq: 0,
        }
    }

    #[test]
    fn handoff_prompt_wraps_plain_transcript() {
        let prompt = build_prompt(&session(), &[message()]);

        assert!(prompt.contains("This is a handoff, not a native resume."));
        assert!(prompt.contains("Session: Fix login bug"));
        assert!(prompt.contains("## User [0]"));
        assert!(prompt.contains("continue this work"));
    }

    #[test]
    fn handoff_commands_cover_first_target_set() {
        let cases = [
            ("codex", "codex", vec!["prompt"]),
            ("grok", "grok", vec!["prompt"]),
            ("claude-code", "claude", vec!["prompt"]),
            ("opencode", "opencode", vec!["run", "-i", "prompt"]),
        ];

        for (target_id, program, args) in cases {
            let target = target_for(target_id).unwrap();
            let command = command_for_target(target, "prompt".to_string());
            assert_eq!(command.program, program);
            assert_eq!(command.args, args);
        }
    }
}
