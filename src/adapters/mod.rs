pub(crate) mod antigravity;
pub(crate) mod claude_code;
pub(crate) mod cline;
pub(crate) mod codex;
pub(crate) mod copilot;
pub(crate) mod copilot_chat;
pub(crate) mod crush;
pub(crate) mod cursor;
pub(crate) mod deepseek_harness;
pub(crate) mod events;
pub(crate) mod factory;
pub(crate) mod file_scan;
pub(crate) mod gemini;
pub(crate) mod goose;
pub(crate) mod grok;
pub(crate) mod invocation_probe;
pub(crate) mod json_util;
pub(crate) mod kilo;
pub(crate) mod kimi_code;
pub(crate) mod kiro;
pub(crate) mod mimo_code;
pub(crate) mod omp;
pub(crate) mod opencode;
pub(crate) mod paths;
pub(crate) mod pi;
pub(crate) mod qwen;
pub(crate) mod roo;
pub(crate) mod sync_state;
pub(crate) mod usage;
pub(crate) mod zcode;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;

use crate::db::store::{
    EventSessionStateMeta, IndexedSessionMeta, MetadataSessionStateMeta, SessionPath,
    UsageSessionStateMeta,
};
use crate::types::{ParentLink, RawSessionEvent, RawUsageEvent, Role, ThreadRole};

pub(crate) trait SourceAdapter {
    fn id(&self) -> &str;
    fn label(&self) -> &str;
    fn scan(&self) -> anyhow::Result<Vec<RawSession>>;
    fn usage_parser_version(&self) -> Option<u32> {
        None
    }
    fn scan_for_sync(
        &self,
        _context: &AdapterSyncContext,
        _since_ts: Option<i64>,
        _include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        Ok(None)
    }
    fn scan_for_sync_output(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
        force: bool,
    ) -> anyhow::Result<Option<SyncScanOutput>> {
        if force {
            return Ok(None);
        }
        Ok(self
            .scan_for_sync(context, since_ts, include_events)?
            .map(|scan| SyncScanOutput { scan, reconcile: None }))
    }
    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand>;
    fn app_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        None
    }
}

pub(crate) struct AdapterSyncContext {
    source: String,
    session_meta: HashMap<String, IndexedSessionMeta>,
    session_paths: HashMap<String, SessionPath>,
    imported_ids: HashSet<String>,
    usage_state: HashMap<String, UsageSessionStateMeta>,
    event_state: HashMap<String, EventSessionStateMeta>,
    metadata_state: HashMap<String, MetadataSessionStateMeta>,
}

pub(crate) struct AdapterSyncContextParts {
    pub(crate) session_meta: HashMap<String, IndexedSessionMeta>,
    pub(crate) session_paths: HashMap<String, SessionPath>,
    pub(crate) imported_ids: HashSet<String>,
    pub(crate) usage_state: HashMap<String, UsageSessionStateMeta>,
    pub(crate) event_state: HashMap<String, EventSessionStateMeta>,
    pub(crate) metadata_state: HashMap<String, MetadataSessionStateMeta>,
}

impl AdapterSyncContext {
    pub(crate) fn new(
        source: String,
        session_meta: HashMap<String, IndexedSessionMeta>,
        session_paths: HashMap<String, SessionPath>,
        imported_ids: HashSet<String>,
        usage_state: HashMap<String, UsageSessionStateMeta>,
        event_state: HashMap<String, EventSessionStateMeta>,
        metadata_state: HashMap<String, MetadataSessionStateMeta>,
    ) -> Self {
        Self {
            source,
            session_meta,
            session_paths,
            imported_ids,
            usage_state,
            event_state,
            metadata_state,
        }
    }

    pub(crate) fn source(&self) -> &str {
        &self.source
    }

    pub(crate) fn session_meta(&self) -> &HashMap<String, IndexedSessionMeta> {
        &self.session_meta
    }

    pub(crate) fn session_paths(&self) -> impl Iterator<Item = &SessionPath> {
        self.session_paths.values()
    }

    pub(crate) fn has_existing_sessions(&self) -> bool {
        !self.session_meta.is_empty()
    }

    pub(crate) fn usage_state(&self) -> &HashMap<String, UsageSessionStateMeta> {
        &self.usage_state
    }

    pub(crate) fn event_state(&self) -> &HashMap<String, EventSessionStateMeta> {
        &self.event_state
    }

    pub(crate) fn metadata_state(&self) -> &HashMap<String, MetadataSessionStateMeta> {
        &self.metadata_state
    }

    pub(crate) fn into_parts(self) -> AdapterSyncContextParts {
        AdapterSyncContextParts {
            session_meta: self.session_meta,
            session_paths: self.session_paths,
            imported_ids: self.imported_ids,
            usage_state: self.usage_state,
            event_state: self.event_state,
            metadata_state: self.metadata_state,
        }
    }

    #[cfg(test)]
    pub(crate) fn empty_for_test(source: &str) -> Self {
        Self::new(
            source.to_string(),
            HashMap::new(),
            HashMap::new(),
            HashSet::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        )
    }

    #[cfg(test)]
    pub(crate) fn from_store_for_test(
        store: &crate::db::store::Store,
        source: &str,
    ) -> anyhow::Result<Self> {
        Ok(Self::new(
            source.to_string(),
            store.session_meta_map(source)?,
            store
                .session_paths_for_source(source)?
                .into_iter()
                .map(|path| (path.source_id.clone(), path))
                .collect(),
            store.imported_source_ids(source)?,
            store.usage_state_meta_map(source)?,
            store.event_state_meta_map(source)?,
            store.metadata_state_meta_map(source)?,
        ))
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
    pub(crate) thread_role: Option<ThreadRole>,
    pub(crate) parent_links: Vec<ParentLink>,
    pub(crate) metadata_parser_version: Option<u32>,
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
            thread_role: None,
            parent_links: Vec::new(),
            metadata_parser_version: None,
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
    pub(crate) unstable_sessions: u32,
    /// Every session the adapter considered, before any filtering. The three
    /// counters below partition it, so `candidates - skipped - filtered -
    /// parsed` is the number an adapter dropped without accounting for it.
    pub(crate) candidates: u32,
    /// Candidates rejected without reading their transcript. This is the
    /// counter a scan-level optimisation has to move; sessions dropped after
    /// parsing cost the same as sessions that were kept.
    pub(crate) rejected_before_parse: u32,
    pub(crate) parsed: u32,
}

pub(crate) struct SyncScanResult {
    pub(crate) sessions: Vec<RawSession>,
    pub(crate) stats: SyncScanStats,
    pub(crate) observations: Vec<SourceObservation>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SourceObservation {
    pub(crate) source_id: String,
    pub(crate) source_file_path: Option<String>,
}

pub(crate) struct SyncScanOutput {
    pub(crate) scan: SyncScanResult,
    pub(crate) reconcile: Option<ReconcilePlan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InventoryIssue {
    pub(crate) path: PathBuf,
    pub(crate) category: io::ErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReconcilePlan {
    CompleteLiveSet(HashSet<String>),
    ExactTombstones(HashSet<String>),
    PartialInventory(Vec<InventoryIssue>),
    UnavailableInventory(Vec<InventoryIssue>),
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
        Box::new(omp::OmpAdapter),
        Box::new(antigravity::AntigravityAdapter),
        Box::new(gemini::GeminiAdapter),
        Box::new(grok::GrokAdapter),
        Box::new(kiro::KiroAdapter),
        Box::new(copilot::CopilotAdapter),
        Box::new(copilot_chat::CopilotChatAdapter),
        Box::new(cursor::CursorAdapter),
        Box::new(cline::ClineAdapter),
        Box::new(roo::RooAdapter),
        Box::new(deepseek_harness::DeepSeekHarnessAdapter),
        Box::new(kimi_code::KimiCodeAdapter),
        Box::new(qwen::QwenAdapter),
        Box::new(kilo::KiloCodeAdapter),
        Box::new(crush::CrushAdapter),
        Box::new(mimo_code::MimoCodeAdapter),
        Box::new(zcode::ZcodeAdapter),
        Box::new(goose::GooseAdapter),
        Box::new(factory::FactoryAdapter),
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
    matches!(
        source_id,
        "codex"
            | "claude-code"
            | "cursor"
            | "copilot-cli"
            | "opencode"
            | "kilo-code"
            | "crush"
            | "mimo-code"
            | "zcode"
            | "goose"
            | "factory"
    )
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

#[cfg(test)]
mod tests {
    use super::all_adapters;

    #[test]
    fn all_adapters_includes_factory() {
        let ids: Vec<_> = all_adapters().iter().map(|adapter| adapter.id().to_string()).collect();
        assert!(ids.iter().any(|id| id == "factory"), "factory missing from all_adapters()");
    }
}
