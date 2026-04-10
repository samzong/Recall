pub mod claude_code;
pub mod codex;
pub mod copilot;
pub mod gemini;
pub mod kiro;
pub mod opencode;

use crate::types::Role;

pub trait SourceAdapter {
    fn id(&self) -> &str;
    fn label(&self) -> &str;
    fn scan(&self) -> anyhow::Result<Vec<RawSession>>;
    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand>;
}

pub struct RawSession {
    pub source_id: String,
    pub directory: Option<String>,
    pub started_at: i64,
    pub updated_at: Option<i64>,
    pub entrypoint: Option<String>,
    pub messages: Vec<RawMessage>,
}

pub struct RawMessage {
    pub role: Role,
    pub content: String,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ResumeCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl ResumeCommand {
    pub fn display(&self) -> String {
        let mut out = self.program.clone();
        for arg in &self.args {
            out.push(' ');
            out.push_str(arg);
        }
        out
    }
}

pub fn all_adapters() -> Vec<Box<dyn SourceAdapter>> {
    vec![
        Box::new(claude_code::ClaudeCodeAdapter),
        Box::new(opencode::OpenCodeAdapter),
        Box::new(codex::CodexAdapter),
        Box::new(gemini::GeminiAdapter),
        Box::new(kiro::KiroAdapter),
        Box::new(copilot::CopilotAdapter),
    ]
}

pub fn resume_command_for(source: &str, source_id: &str) -> Option<ResumeCommand> {
    all_adapters().iter().find(|a| a.id() == source).and_then(|a| a.resume_command(source_id))
}

pub fn source_labels() -> Vec<(String, String)> {
    all_adapters().iter().map(|a| (a.id().to_string(), a.label().to_string())).collect()
}
