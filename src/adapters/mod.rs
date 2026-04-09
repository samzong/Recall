pub mod claude_code;
pub mod codex;
pub mod opencode;

use crate::types::Role;

pub trait SourceAdapter {
    fn id(&self) -> &str;
    fn label(&self) -> &str;
    fn scan(&self) -> anyhow::Result<Vec<RawSession>>;
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

pub fn all_adapters() -> Vec<Box<dyn SourceAdapter>> {
    vec![
        Box::new(claude_code::ClaudeCodeAdapter),
        Box::new(opencode::OpenCodeAdapter),
        Box::new(codex::CodexAdapter),
    ]
}

pub fn source_labels() -> Vec<(String, String)> {
    vec![
        ("claude-code".to_string(), "CC".to_string()),
        ("opencode".to_string(), "OC".to_string()),
        ("codex".to_string(), "CDX".to_string()),
    ]
}
