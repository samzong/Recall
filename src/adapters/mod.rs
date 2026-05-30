pub mod antigravity;
pub mod claude_code;
pub mod cline;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod events;
pub mod file_scan;
pub mod gemini;
pub mod kiro;
pub mod opencode;
pub mod pi;

use crate::db::store::Store;
use crate::types::{RawSessionEvent, RawUsageEvent, Role};

pub trait SourceAdapter {
    fn id(&self) -> &str;
    fn label(&self) -> &str;
    fn scan(&self) -> anyhow::Result<Vec<RawSession>>;
    fn scan_summary(&self) -> anyhow::Result<Option<SourceScanSummary>> {
        Ok(None)
    }
    fn usage_parser_version(&self) -> Option<u32> {
        None
    }
    fn scan_for_sync(
        &self,
        _store: &Store,
        _since_ts: Option<i64>,
        _include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        Ok(None)
    }
    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand>;
    fn app_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        None
    }
}

pub struct RawSession {
    pub source_id: String,
    pub directory: Option<String>,
    pub started_at: i64,
    pub updated_at: Option<i64>,
    pub entrypoint: Option<String>,
    pub messages: Vec<RawMessage>,
    pub usage_events: Vec<RawUsageEvent>,
    pub usage_parser_version: Option<u32>,
    pub events: Vec<RawSessionEvent>,
    pub event_parser_version: Option<u32>,
}

impl RawSession {
    pub fn search_only(
        source_id: impl Into<String>,
        directory: Option<String>,
        started_at: i64,
        updated_at: Option<i64>,
        entrypoint: Option<String>,
        messages: Vec<RawMessage>,
    ) -> Self {
        Self {
            source_id: source_id.into(),
            directory,
            started_at,
            updated_at,
            entrypoint,
            messages,
            usage_events: Vec::new(),
            usage_parser_version: None,
            events: Vec::new(),
            event_parser_version: None,
        }
    }

    pub fn with_usage(mut self, usage_events: Vec<RawUsageEvent>, parser_version: u32) -> Self {
        self.usage_events = usage_events;
        self.usage_parser_version = Some(parser_version);
        self
    }

    pub fn with_events(mut self, events: Vec<RawSessionEvent>, parser_version: u32) -> Self {
        self.events = events;
        self.event_parser_version = Some(parser_version);
        self
    }
}

pub struct RawMessage {
    pub role: Role,
    pub content: String,
    pub timestamp: Option<i64>,
}

#[derive(Default)]
pub struct SyncScanStats {
    pub skipped_sessions: u32,
    pub filtered_sessions: u32,
}

pub struct SyncScanResult {
    pub sessions: Vec<RawSession>,
    pub stats: SyncScanStats,
}

pub struct SourceScanSummary {
    pub sessions: usize,
    pub messages: usize,
    pub oldest_started_at: Option<i64>,
    pub newest_started_at: Option<i64>,
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
        Box::new(pi::PiAdapter),
        Box::new(antigravity::AntigravityAdapter),
        Box::new(gemini::GeminiAdapter),
        Box::new(kiro::KiroAdapter),
        Box::new(copilot::CopilotAdapter),
        Box::new(cursor::CursorAdapter),
        Box::new(cline::ClineAdapter),
    ]
}

pub fn resume_command_for(source: &str, source_id: &str) -> Option<ResumeCommand> {
    all_adapters().iter().find(|a| a.id() == source).and_then(|a| a.resume_command(source_id))
}

pub fn app_command_for(source: &str, source_id: &str) -> Option<ResumeCommand> {
    all_adapters().iter().find(|a| a.id() == source).and_then(|a| a.app_command(source_id))
}

pub fn source_labels() -> Vec<(String, String)> {
    all_adapters().iter().map(|a| (a.id().to_string(), a.label().to_string())).collect()
}

pub fn source_supports_event_backfill(source_id: &str) -> bool {
    matches!(source_id, "codex" | "claude-code" | "cursor" | "copilot-cli" | "opencode")
}

pub fn adapter_supports_usage_dashboard(
    adapter: &dyn SourceAdapter,
    backfill_events: bool,
) -> bool {
    if adapter.usage_parser_version().is_some() {
        return true;
    }
    backfill_events && source_supports_event_backfill(adapter.id())
}

pub fn dashboard_source_labels() -> Vec<(String, String)> {
    all_adapters()
        .iter()
        .filter(|adapter| adapter_supports_usage_dashboard(adapter.as_ref(), true))
        .map(|adapter| (adapter.id().to_string(), adapter.label().to_string()))
        .collect()
}
