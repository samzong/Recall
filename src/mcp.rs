mod current_session;
mod discovery;
mod file_history;
mod read_session;

use current_session::*;
use discovery::*;
use file_history::*;
use read_session::*;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use chrono::SecondsFormat;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo, Tool};
use rmcp::{
    ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router, transport::stdio,
};
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::adapters;
use crate::db::message_store::{MessageRead, MessageWindow};
use crate::db::search::{
    FileHistoryCoverage, FileHistoryEvidence, FileHistoryQuery, FileHistoryTarget, SearchEngine,
    SearchFilters, SessionEventQuery, TimeRange,
};
use crate::db::store::Store;
use crate::project_scope::ProjectScope;
use crate::query::{query_embedding, resolve_source_filter};
use crate::types::{EvidenceVisibility, Message, Session, SessionEventRecord};

const EXCERPT_CHAR_CAP: usize = 200;

const MISSING_INDEX: &str =
    "Recall index not found. Run `recall sync` in a terminal to create it, then retry.";
const EMPTY_INDEX: &str = "No sessions in the Recall index. Run `recall sync` in a terminal after using a supported coding agent.";
const SEARCH_EMPTY: &str = "No matching sessions.";
const SESSION_NOT_FOUND: &str = "Session not found.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum McpCapabilitiesFormat {
    Text,
    Json,
}

#[derive(Serialize)]
struct McpCapabilities {
    server: ServerInfo,
    tools: Vec<Tool>,
}

enum IndexState {
    Ready(Store),
    Unavailable { path: Option<PathBuf>, message: String },
}

#[derive(Clone)]
struct RecallMcp {
    index: Arc<Mutex<IndexState>>,
    current_session: CurrentSessionContext,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl IndexState {
    fn open(db: Option<&Path>) -> Self {
        let path = match db {
            Some(path) => path.to_path_buf(),
            None => match Store::default_db_path() {
                Ok(path) => path,
                Err(error) => {
                    return Self::Unavailable { path: None, message: error.to_string() };
                }
            },
        };
        Self::open_path(path)
    }

    fn open_path(path: PathBuf) -> Self {
        if !path.exists() {
            let message = format!("{MISSING_INDEX} ({})", path.display());
            return Self::Unavailable { path: Some(path), message };
        }
        match Store::open_read_only_at(&path) {
            Ok(store) => Self::Ready(store),
            Err(error) => {
                let message = format!("Cannot open Recall index at {}: {error}", path.display());
                Self::Unavailable { path: Some(path), message }
            }
        }
    }

    fn ensure_open(&mut self) {
        let path = match self {
            Self::Ready(_) => return,
            Self::Unavailable { path: Some(path), .. } => path.clone(),
            Self::Unavailable { path: None, .. } => {
                *self = Self::open(None);
                return;
            }
        };
        *self = Self::open_path(path);
    }
}

pub(crate) fn run(db: Option<PathBuf>) -> Result<()> {
    let runtime =
        tokio::runtime::Builder::new_current_thread().enable_io().enable_time().build()?;
    runtime.block_on(serve(db))
}

pub(crate) fn run_capabilities(format: McpCapabilitiesFormat) -> Result<()> {
    let report = mcp_capabilities();
    match format {
        McpCapabilitiesFormat::Text => print!("{}", render_capabilities(&report)),
        McpCapabilitiesFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
}

async fn serve(db: Option<PathBuf>) -> Result<()> {
    let service = RecallMcp::new(db.as_deref()).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[tool_router]
impl RecallMcp {
    fn new(db: Option<&Path>) -> Self {
        Self {
            index: Arc::new(Mutex::new(IndexState::open(db))),
            current_session: CurrentSessionContext::from_env(),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Search past AI coding sessions across Claude Code, Codex, OpenCode, Cursor, and other indexed tools. Use when the user asks whether they have seen an error, made a decision, or solved a similar problem before. Pass a fresh invocation_nonce when the host does not expose a session ID. current_session.resolution is resolved only after exact Store verification; only then is that session excluded before ranking and limit. Unknown leaves results unchanged. Returns Recall session_id, source-native source_session_id, source, project, title, matched excerpt, and ISO-8601 timestamp.",
        annotations(
            title = "Search sessions",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn search_sessions(
        &self,
        Parameters(args): Parameters<SearchSessionsArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        Ok(json_result(search_sessions_with_context(
            &lock_index(&self.index),
            &args,
            &self.current_session,
        )))
    }

    #[tool(
        description = "Search message text by keywords and return individual matches with session_id, source_session_id, source, title, seq, role, timestamp (Unix milliseconds), and an excerpt around the match. This uses full-text search, not embeddings. Prefer this for specific historical evidence; use get_session with around_seq set to a returned seq for context. Limit defaults to 10, maximum 50. Pass project for project-scoped discovery and a fresh invocation_nonce for self-exclusion. Explicit session_id searches include that session even when it is the invoking session. Sequence anchors refer to the current index, not permanent source identities.",
        annotations(
            title = "Search messages",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn search_messages(
        &self,
        Parameters(args): Parameters<SearchMessagesArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        Ok(json_result(
            search_message_matches(&lock_index(&self.index), &args, &self.current_session)
                .map_err(|message| serde_json::json!({"message": message, "matches": []})),
        ))
    }

    #[tool(
        description = "Read indexed evidence with event_ref copied from file_history: returns lossless UTF-8 chunks, a continuation cursor, and a content digest. Default evidence_part=payload includes full indexed attrs and same-session related refs; concatenate data before parsing its JSON. For native Cursor before/after content, first read payload.related_event_refs and select the result event holding beforeContentId/afterContentId; content is opt-in and hash-verified. max_bytes defaults to 16384, maximum 65536; each call has a 64 MiB read budget. Copy event_ref, evidence_part, session_id and cursor to continue. Read the returned discussion window separately. Without event_ref, Load one indexed session by Recall session_id. After search_messages, pass around_seq to read the matched message and its neighbors (default three before and after). Alternatively use inclusive from_seq/to_seq. These selectors cannot be combined with tail. Selected reads return up to 50 messages and 6000 Unicode content characters, with first_message_byte_offset and next_cursor for lossless continuation. Resume using session_id and cursor without selectors; reindexing invalidates cursors. Without selectors, cursor, or max_chars, the legacy head/tail behavior follows. Use after search_sessions or list_recent_sessions when you need the conversation itself. Each message is truncated at 2000 characters; the combined message text is capped at 32000 bytes. Set include_events only when structured evidence is needed; it returns at most 50 events anchored to the returned message range plus unanchored events, caps each event string field at 200 characters and all event string fields at 10000 characters total, and never returns raw arguments, results, source paths, or parser internals. Returns metadata, including source-native source_session_id and the first_message_seq and last_message_seq represented by the response, plus messages as plain text in sequence order.",
        annotations(
            title = "Get session",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn get_session(
        &self,
        Parameters(args): Parameters<GetSessionArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if args.event_ref.is_some() {
            return Ok(json_result(
                get_event_evidence(&lock_index(&self.index), &args)
                    .map_err(|message| serde_json::json!({"message":message})),
            ));
        }
        Ok(json_result(
            get_session(&lock_index(&self.index), &args)
                .map_err(|message| empty_detail(Some(message))),
        ))
    }

    #[tool(
        description = "List the most recently active indexed sessions, newest first. Use to browse what the user has been working on when there is no search query. Pass a fresh invocation_nonce when the host does not expose a session ID. current_session.resolution is resolved only after exact Store verification; only then is that session excluded before ordering and limit. Unknown leaves results unchanged. Each hit has the same shape as search_sessions: Recall session_id, source-native source_session_id, source, project, title, excerpt (summary when present), and ISO-8601 timestamp.",
        annotations(
            title = "List recent sessions",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn list_recent_sessions(
        &self,
        Parameters(args): Parameters<ListRecentSessionsArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        Ok(json_result(list_recent_sessions_with_context(
            &lock_index(&self.index),
            &args,
            &self.current_session,
        )))
    }

    #[tool(
        description = "Which indexed sessions touched a file. An event matches when its target equals the given path, or when either one ends with `/` or `\\` followed by the other, so relative and absolute forms of the same path find each other. Defaults to file_write and file_read events. Returns session id, source, project, title, kind, name, target, event_seq, truncated summary, event timestamp, and optional visibility and is_meta flags. Set target_project to explicitly use structured target-file evidence across session projects; it is mutually exclusive with project. This mode matches exact file identities, includes all evidence kinds by default, and returns evidence classifications, event_ref, coverage on the first page, and continuation. include_command_candidates and cursor require this mode. Command evidence and approval do not prove a write succeeded. Unknown event timestamps sort after known timestamps; ties use indexed event identity. Target-relevant changes invalidate continuation. No sync or native source-file read is performed.",
        annotations(
            title = "File history",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn file_history(
        &self,
        Parameters(args): Parameters<FileHistoryArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        Ok(json_result(file_history(&lock_index(&self.index), &args)))
    }
}

#[cfg(feature = "bench")]
pub(crate) struct GetSessionBenchmark {
    server: RecallMcp,
    max_messages: u32,
    tail: bool,
}

#[cfg(feature = "bench")]
impl GetSessionBenchmark {
    pub(crate) fn new(store: Store, max_messages: u32, tail: bool) -> Self {
        Self {
            server: RecallMcp {
                index: Arc::new(Mutex::new(IndexState::Ready(store))),
                current_session: CurrentSessionContext::default(),
                tool_router: RecallMcp::tool_router(),
            },
            max_messages,
            tail,
        }
    }

    pub(crate) fn run(&self) -> Vec<u8> {
        let result = self
            .server
            .get_session(Parameters(GetSessionArgs {
                session_id: "claude-code:session-0".to_string(),
                max_messages: Some(self.max_messages),
                tail: self.tail,
                include_events: false,
                ..Default::default()
            }))
            .expect("benchmark get_session");
        serde_json::to_vec(&result).expect("serialize benchmark get_session")
    }
}

#[tool_handler]
impl ServerHandler for RecallMcp {
    fn get_info(&self) -> ServerInfo {
        mcp_server_info()
    }
}

fn mcp_server_info() -> ServerInfo {
    ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        .with_server_info(Implementation::new("recall", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "Read-only memory over the local Recall session index. Search or list past coding sessions, look up which sessions touched a file, then fetch a session when you need the transcript. Discovery tools report whether the current session was resolved and excluded; pass a fresh invocation_nonce when the host provides no session identity. Unknown resolution never guesses or changes results. This server never writes, syncs, waits for transcripts, or mutates the index.",
        )
}

fn mcp_capabilities() -> McpCapabilities {
    McpCapabilities { server: mcp_server_info(), tools: RecallMcp::tool_router().list_all() }
}

fn render_capabilities(report: &McpCapabilities) -> String {
    let mut output = format!("Recall MCP {}\n", env!("CARGO_PKG_VERSION"));
    if let Some(instructions) = report.server.instructions.as_deref() {
        output.push_str(instructions);
        output.push('\n');
    }
    let capabilities = serde_json::to_value(&report.server.capabilities)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .map(|values| values.into_iter().map(|(key, _)| key).collect::<Vec<_>>().join(", "))
        .unwrap_or_default();
    if !capabilities.is_empty() {
        output.push_str("Capabilities: ");
        output.push_str(&capabilities);
        output.push('\n');
    }
    for tool in &report.tools {
        output.push('\n');
        output.push_str(tool.name.as_ref());
        output.push('\n');
        if let Some(description) = tool.description.as_deref() {
            output.push_str(description);
            output.push('\n');
        }
        let inputs = tool
            .input_schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .map(|values| values.keys().cloned().collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        if !inputs.is_empty() {
            output.push_str("Inputs: ");
            output.push_str(&inputs);
            output.push('\n');
        }
    }
    output
}

fn lock_index(index: &Mutex<IndexState>) -> std::sync::MutexGuard<'_, IndexState> {
    let mut guard = index.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.ensure_open();
    guard
}

fn json_result(result: std::result::Result<impl Serialize, impl Serialize>) -> CallToolResult {
    match result {
        Ok(value) => CallToolResult::structured(to_json(value)),
        Err(value) => CallToolResult::structured_error(to_json(value)),
    }
}

fn to_json(value: impl Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn empty_hits(message: impl Into<String>) -> HitList {
    HitList {
        message: Some(message.into()),
        hits: Vec::new(),
        current_session: CurrentSession::unknown(),
    }
}

fn empty_hits_with_current(message: impl Into<String>, current_session: CurrentSession) -> HitList {
    HitList { message: Some(message.into()), hits: Vec::new(), current_session }
}

#[cfg(test)]
fn search_sessions(
    index: &IndexState,
    args: &SearchSessionsArgs,
) -> std::result::Result<HitList, Box<HitList>> {
    search_sessions_with_context(index, args, &CurrentSessionContext::default())
}

fn search_sessions_with_context(
    index: &IndexState,
    args: &SearchSessionsArgs,
    context: &CurrentSessionContext,
) -> std::result::Result<HitList, Box<HitList>> {
    match index {
        IndexState::Unavailable { message, .. } => Err(Box::new(empty_hits(message.clone()))),
        IndexState::Ready(store) => {
            let current_session = context.resolve(store, args.invocation_nonce.as_deref());
            search_ready(store, args, &current_session)
                .map_err(|message| Box::new(empty_hits_with_current(message, current_session)))
        }
    }
}

fn search_message_matches(
    index: &IndexState,
    args: &SearchMessagesArgs,
    context: &CurrentSessionContext,
) -> std::result::Result<Value, String> {
    let store = match index {
        IndexState::Ready(store) => store,
        IndexState::Unavailable { message, .. } => return Err(message.clone()),
    };
    if let Some(id) = args.session_id.as_deref()
        && store.get_session_by_id(id).map_err(|e| e.to_string())?.is_none()
    {
        return Err(format!("{SESSION_NOT_FOUND} {id}"));
    }
    let current = context.resolve(store, args.search.invocation_nonce.as_deref());
    let (scope, sources) =
        resolve_filters(store, args.search.project.as_deref(), args.search.source.as_deref())?;
    let filters = SearchFilters {
        sources,
        scope,
        time_range: TimeRange::All,
        thread_role: None,
        excluded_session_id: if args.session_id.is_none() {
            current.session_id.clone()
        } else {
            None
        },
    };
    let matches = SearchEngine::new(&store.conn)
        .search_messages(
            &args.search.query,
            &filters,
            args.session_id.as_deref(),
            clamp_limit(args.search.limit, SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX),
        )
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"matches": matches, "current_session": current,
        "current_session_excluded": filters.excluded_session_id.is_some()}))
}

#[cfg(test)]
fn list_recent_sessions(
    index: &IndexState,
    args: &ListRecentSessionsArgs,
) -> std::result::Result<HitList, Box<HitList>> {
    list_recent_sessions_with_context(index, args, &CurrentSessionContext::default())
}

fn list_recent_sessions_with_context(
    index: &IndexState,
    args: &ListRecentSessionsArgs,
    context: &CurrentSessionContext,
) -> std::result::Result<HitList, Box<HitList>> {
    match index {
        IndexState::Unavailable { message, .. } => Err(Box::new(empty_hits(message.clone()))),
        IndexState::Ready(store) => {
            let current_session = context.resolve(store, args.invocation_nonce.as_deref());
            list_ready(store, args, &current_session)
                .map_err(|message| Box::new(empty_hits_with_current(message, current_session)))
        }
    }
}

fn file_history(
    index: &IndexState,
    args: &FileHistoryArgs,
) -> std::result::Result<EventList, Box<EventList>> {
    match index {
        IndexState::Unavailable { message, .. } => Err(Box::new(empty_events(message.clone()))),
        IndexState::Ready(store) => {
            file_history_ready(store, args).map_err(|message| Box::new(empty_events(message)))
        }
    }
}

fn opt_trimmed(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn get_session(
    index: &IndexState,
    args: &GetSessionArgs,
) -> std::result::Result<SessionDetail, String> {
    match index {
        IndexState::Unavailable { message, .. } => Err(message.clone()),
        IndexState::Ready(store) => get_ready(store, args),
    }
}

fn get_event_evidence(
    index: &IndexState,
    args: &GetSessionArgs,
) -> std::result::Result<crate::event_evidence::EvidencePage, String> {
    let IndexState::Ready(store) = index else {
        return Err(MISSING_INDEX.into());
    };
    if args.tail
        || args.include_events
        || args.from_seq.is_some()
        || args.to_seq.is_some()
        || args.around_seq.is_some()
        || args.before.is_some()
        || args.after.is_some()
        || args.max_messages.is_some()
        || args.max_chars.is_some()
    {
        return Err(
            "event_ref cannot be combined with message selectors; read discussion separately"
                .into(),
        );
    }
    crate::event_evidence::read(
        store,
        &args.session_id,
        args.event_ref.as_deref().ok_or_else(|| "event_ref is required".to_string())?,
        args.evidence_part.unwrap_or_default(),
        args.cursor.as_deref(),
        args.max_bytes.unwrap_or(16384) as usize,
    )
    .map_err(|error| error.to_string())
}

fn resolve_filters(
    store: &Store,
    project: Option<&str>,
    source: Option<&str>,
) -> std::result::Result<(ProjectScope, Option<Vec<String>>), String> {
    let scope = match project.map(str::trim).filter(|value| !value.is_empty()) {
        Some(project) => {
            store.resolve_project_selector(project).map_err(|error| error.to_string())?
        }
        None => ProjectScope::Global,
    };
    let sources = resolve_source_filter(source, &adapters::source_labels())
        .map_err(|error| error.to_string())?;
    Ok((scope, sources))
}

fn store_is_empty(store: &Store) -> bool {
    store.stats().ok().is_some_and(|(sessions, _)| sessions == 0)
}

fn session_hit(session: &Session, excerpt: Option<String>) -> SessionHit {
    SessionHit {
        session_id: session.id.clone(),
        source_session_id: session.source_id.clone(),
        source: session.source.clone(),
        project: session.directory.clone(),
        title: session_title(session),
        excerpt,
        timestamp: iso8601(session.started_at),
        locations: session.locations.clone(),
        alternative_versions: session.alternative_versions,
    }
}

fn session_title(session: &Session) -> String {
    session
        .custom_title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or(session.title.as_str())
        .to_string()
}

fn iso8601(millis: i64) -> String {
    chrono::DateTime::from_timestamp_millis(millis)
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".to_string())
}

fn clamp_limit(limit: Option<u32>, default: u32, max: u32) -> usize {
    usize::try_from(limit.unwrap_or(default).clamp(1, max)).unwrap_or(max as usize)
}

fn truncate_chars(text: &str, max_chars: usize) -> (String, bool) {
    if max_chars == 0 {
        return (String::new(), !text.is_empty());
    }
    let mut chars = text.chars();
    let kept: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_none() {
        (kept, false)
    } else if max_chars == 1 {
        ("…".to_string(), true)
    } else {
        let mut trimmed: String = kept.chars().take(max_chars - 1).collect();
        trimmed.push('…');
        (trimmed, true)
    }
}

fn deserialize_opt_u32<'de, D>(deserializer: D) -> std::result::Result<Option<u32>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumberOrString {
        U(u32),
        I(i64),
        S(String),
        Null,
    }

    match Option::<NumberOrString>::deserialize(deserializer)? {
        None | Some(NumberOrString::Null) => Ok(None),
        Some(NumberOrString::U(value)) => Ok(Some(value)),
        Some(NumberOrString::I(value)) if value >= 0 => {
            u32::try_from(value).map(Some).map_err(de::Error::custom)
        }
        Some(NumberOrString::S(value)) if value.trim().is_empty() => Ok(None),
        Some(NumberOrString::S(value)) => value.parse().map(Some).map_err(de::Error::custom),
        Some(NumberOrString::I(_)) => Err(de::Error::custom("expected a non-negative integer")),
    }
}
