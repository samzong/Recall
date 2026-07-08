pub(crate) mod antigravity;
pub(crate) mod claude_code;
pub(crate) mod cline;
pub(crate) mod codex;
pub(crate) mod copilot;
pub(crate) mod cursor;
pub(crate) mod events;
pub(crate) mod file_scan;
pub(crate) mod gemini;
pub(crate) mod grok;
pub(crate) mod json_util;
pub(crate) mod kiro;
pub(crate) mod opencode;
pub(crate) mod paths;
pub(crate) mod pi;
pub(crate) mod sync_state;

use crate::db::store::Store;
use crate::types::{RawSessionEvent, RawUsageEvent, Role};

pub(crate) trait SourceAdapter {
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
    fn prune(&self, _store: &Store) -> anyhow::Result<()> {
        Ok(())
    }
    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand>;
    fn app_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        None
    }
}

pub(crate) struct RawSession {
    pub(crate) source_id: String,
    pub(crate) directory: Option<String>,
    pub(crate) started_at: i64,
    pub(crate) updated_at: Option<i64>,
    pub(crate) entrypoint: Option<String>,
    pub(crate) messages: Vec<RawMessage>,
    pub(crate) usage_events: Vec<RawUsageEvent>,
    pub(crate) usage_parser_version: Option<u32>,
    pub(crate) events: Vec<RawSessionEvent>,
    pub(crate) event_parser_version: Option<u32>,
    pub(crate) source_file_path: Option<String>,
    pub(crate) custom_title: Option<String>,
    pub(crate) summary: Option<String>,
    pub(crate) duration_minutes: Option<u32>,
}

impl RawSession {
    pub(crate) fn search_only(
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
            source_file_path: None,
            custom_title: None,
            summary: None,
            duration_minutes: None,
        }
    }

    pub(crate) fn with_usage(
        mut self,
        usage_events: Vec<RawUsageEvent>,
        parser_version: u32,
    ) -> Self {
        self.usage_events = usage_events;
        self.usage_parser_version = Some(parser_version);
        self
    }

    pub(crate) fn with_events(mut self, events: Vec<RawSessionEvent>, parser_version: u32) -> Self {
        self.events = events;
        self.event_parser_version = Some(parser_version);
        self
    }
}

pub(crate) struct RawMessage {
    pub(crate) role: Role,
    pub(crate) content: String,
    pub(crate) timestamp: Option<i64>,
}

pub(crate) fn first_timestamp(
    meta: Option<i64>,
    messages: &[RawMessage],
    usage_events: &[RawUsageEvent],
    events: &[RawSessionEvent],
) -> Option<i64> {
    meta.or_else(|| messages.first().and_then(|message| message.timestamp))
        .or_else(|| usage_events.first().map(|event| event.timestamp))
        .or_else(|| events.first().and_then(|event| event.timestamp))
}

pub(crate) fn last_timestamp(
    meta: Option<i64>,
    messages: &[RawMessage],
    usage_events: &[RawUsageEvent],
    events: &[RawSessionEvent],
) -> Option<i64> {
    meta.or_else(|| messages.last().and_then(|message| message.timestamp))
        .or_else(|| usage_events.last().map(|event| event.timestamp))
        .or_else(|| events.last().and_then(|event| event.timestamp))
}

#[derive(Default)]
pub(crate) struct SyncScanStats {
    pub(crate) skipped_sessions: u32,
    pub(crate) filtered_sessions: u32,
}

pub(crate) struct SyncScanResult {
    pub(crate) sessions: Vec<RawSession>,
    pub(crate) stats: SyncScanStats,
}

pub(crate) struct SourceScanSummary {
    pub(crate) sessions: usize,
    pub(crate) messages: usize,
    pub(crate) oldest_started_at: Option<i64>,
    pub(crate) newest_started_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub(crate) struct ResumeCommand {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
}

impl ResumeCommand {
    pub(crate) fn display(&self) -> String {
        let mut out = self.program.clone();
        for arg in &self.args {
            out.push(' ');
            out.push_str(arg);
        }
        out
    }
}

pub(crate) fn all_adapters() -> Vec<Box<dyn SourceAdapter>> {
    vec![
        Box::new(claude_code::ClaudeCodeAdapter),
        Box::new(opencode::OpenCodeAdapter),
        Box::new(codex::CodexAdapter),
        Box::new(pi::PiAdapter),
        Box::new(antigravity::AntigravityAdapter),
        Box::new(gemini::GeminiAdapter),
        Box::new(grok::GrokAdapter),
        Box::new(kiro::KiroAdapter),
        Box::new(copilot::CopilotAdapter),
        Box::new(cursor::CursorAdapter),
        Box::new(cline::ClineAdapter),
    ]
}

pub(crate) fn resume_command_for(source: &str, source_id: &str) -> Option<ResumeCommand> {
    all_adapters().iter().find(|a| a.id() == source).and_then(|a| a.resume_command(source_id))
}

pub(crate) fn app_command_for(source: &str, source_id: &str) -> Option<ResumeCommand> {
    all_adapters().iter().find(|a| a.id() == source).and_then(|a| a.app_command(source_id))
}

pub(crate) fn source_labels() -> Vec<(String, String)> {
    all_adapters().iter().map(|a| (a.id().to_string(), a.label().to_string())).collect()
}

pub(crate) fn source_supports_event_backfill(source_id: &str) -> bool {
    matches!(source_id, "codex" | "claude-code" | "cursor" | "copilot-cli" | "opencode")
}

pub(crate) fn adapter_supports_usage_dashboard(
    adapter: &dyn SourceAdapter,
    backfill_events: bool,
) -> bool {
    if adapter.usage_parser_version().is_some() {
        return true;
    }
    backfill_events && source_supports_event_backfill(adapter.id())
}

pub(crate) fn dashboard_source_labels() -> Vec<(String, String)> {
    all_adapters()
        .iter()
        .filter(|adapter| adapter_supports_usage_dashboard(adapter.as_ref(), true))
        .map(|adapter| (adapter.id().to_string(), adapter.label().to_string()))
        .collect()
}
