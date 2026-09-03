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
use crate::db::search::{SearchEngine, SearchFilters, SessionEventQuery, TimeRange};
use crate::db::store::Store;
use crate::project_scope::ProjectScope;
use crate::query::{query_embedding, resolve_source_filter};
use crate::types::{EvidenceVisibility, Message, Role, Session, SessionEventRecord};

const SEARCH_LIMIT_DEFAULT: u32 = 10;
const SEARCH_LIMIT_MAX: u32 = 50;
const LIST_LIMIT_DEFAULT: u32 = 10;
const LIST_LIMIT_MAX: u32 = 50;
const EVENT_LIMIT_DEFAULT: u32 = 20;
const EVENT_LIMIT_MAX: u32 = 50;
const FILE_HISTORY_KINDS: [&str; 2] = ["file_write", "file_read"];
const GET_MAX_MESSAGES_DEFAULT: u32 = 50;
const GET_MESSAGE_CHAR_CAP: usize = 2_000;
const GET_RESPONSE_CHAR_CAP: usize = 32_000;
const GET_EVENT_LIMIT: usize = 50;
const GET_EVENT_FIELD_CHAR_CAP: usize = 200;
const GET_EVENT_TEXT_CHAR_CAP: usize = 10_000;
const EXCERPT_CHAR_CAP: usize = 200;

const MISSING_INDEX: &str =
    "Recall index not found. Run `recall sync` in a terminal to create it, then retry.";
const EMPTY_INDEX: &str = "No sessions in the Recall index. Run `recall sync` in a terminal after using a supported coding agent.";
const SEARCH_EMPTY: &str = "No matching sessions.";
const SESSION_NOT_FOUND: &str = "Session not found.";
const PATH_REQUIRED: &str = "path is required.";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SearchSessionsArgs {
    /// Required free-text query.
    query: String,
    /// Optional absolute indexed directory path, or a repo name/owner-repo
    /// slug/remote URL derived from git identity. A worktree's own directory
    /// name alone (e.g. "myrepo--feature") does not match.
    #[serde(default)]
    project: Option<String>,
    /// Optional tool id or label such as claude-code or CUR.
    #[serde(default)]
    source: Option<String>,
    /// Maximum hits to return. Defaults to 10, capped at 50.
    #[serde(default, deserialize_with = "deserialize_opt_u32")]
    #[schemars(range(min = 1, max = 50))]
    limit: Option<u32>,
    #[serde(default)]
    #[schemars(
        description = "Optional unique literal copied into this tool call so Recall can verify and exclude the invoking session when no host identity is available. It never affects relevance."
    )]
    invocation_nonce: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetSessionArgs {
    /// Recall session id from search_sessions or list_recent_sessions.
    session_id: String,
    #[serde(default, deserialize_with = "deserialize_opt_u32")]
    #[schemars(
        description = "Maximum messages to return. Defaults to 50. Reads from the start unless tail is true."
    )]
    max_messages: Option<u32>,
    #[serde(default)]
    #[schemars(
        description = "Return the newest messages instead of the oldest, preserving sequence order."
    )]
    tail: bool,
    #[serde(default)]
    #[schemars(
        description = "Include up to 50 structured events anchored to the returned message range, plus unanchored events. Each event string field is capped at 200 characters and all event string fields at 10000 characters total. Defaults to false."
    )]
    include_events: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListRecentSessionsArgs {
    /// Optional absolute indexed directory path, or a repo name/owner-repo
    /// slug/remote URL derived from git identity. A worktree's own directory
    /// name alone (e.g. "myrepo--feature") does not match.
    #[serde(default)]
    project: Option<String>,
    /// Optional tool id or label such as claude-code or CUR.
    #[serde(default)]
    source: Option<String>,
    /// Maximum hits to return. Defaults to 10, capped at 50.
    #[serde(default, deserialize_with = "deserialize_opt_u32")]
    #[schemars(range(min = 1, max = 50))]
    limit: Option<u32>,
    #[serde(default)]
    #[schemars(
        description = "Optional unique literal copied into this tool call so Recall can verify and exclude the invoking session when no host identity is available."
    )]
    invocation_nonce: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct FileHistoryArgs {
    /// File path, relative or absolute. Matches an event when the shorter of
    /// the two is a whole path suffix of the other, so an absolute path also
    /// finds events indexed under a project-relative path and vice versa.
    path: String,
    /// Optional absolute indexed directory path, or a repo name/owner-repo
    /// slug/remote URL derived from git identity. A worktree's own directory
    /// name alone (e.g. "myrepo--feature") does not match.
    #[serde(default)]
    project: Option<String>,
    /// Optional tool id or label such as claude-code or CUR.
    #[serde(default)]
    source: Option<String>,
    /// Optional event kind. When omitted, only file_write and file_read.
    #[serde(default)]
    kind: Option<String>,
    /// Maximum events to return. Defaults to 20, capped at 50.
    #[serde(default, deserialize_with = "deserialize_opt_u32")]
    #[schemars(range(min = 1, max = 50))]
    limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
struct SessionHit {
    session_id: String,
    source_session_id: String,
    source: String,
    project: Option<String>,
    title: String,
    excerpt: Option<String>,
    timestamp: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
struct HitList {
    message: Option<String>,
    hits: Vec<SessionHit>,
    current_session: CurrentSession,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CurrentSessionResolution {
    Resolved,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
struct CurrentSession {
    resolution: CurrentSessionResolution,
    session_id: Option<String>,
    source: Option<String>,
    source_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceSessionIdentity {
    source: String,
    source_session_id: String,
}

#[derive(Clone, Default)]
struct CurrentSessionContext {
    host_identity: Option<SourceSessionIdentity>,
}

impl CurrentSession {
    fn unknown() -> Self {
        Self {
            resolution: CurrentSessionResolution::Unknown,
            session_id: None,
            source: None,
            source_session_id: None,
        }
    }

    fn resolved(session: &Session) -> Self {
        Self {
            resolution: CurrentSessionResolution::Resolved,
            session_id: Some(session.id.clone()),
            source: Some(session.source.clone()),
            source_session_id: Some(session.source_id.clone()),
        }
    }
}

impl CurrentSessionContext {
    fn from_env() -> Self {
        let claude = std::env::var("CLAUDE_CODE_SESSION_ID").ok();
        let codex_thread = std::env::var("CODEX_THREAD_ID").ok();
        let codex_session = std::env::var("CODEX_SESSION_ID").ok();
        Self::from_values(claude.as_deref(), codex_thread.as_deref(), codex_session.as_deref())
    }

    fn from_values(
        claude_session: Option<&str>,
        codex_thread: Option<&str>,
        codex_session: Option<&str>,
    ) -> Self {
        let claude_identity =
            verified_session_id(claude_session).map(|source_session_id| SourceSessionIdentity {
                source: "claude-code".to_string(),
                source_session_id: source_session_id.to_string(),
            });
        let codex_identity =
            match (verified_session_id(codex_thread), verified_session_id(codex_session)) {
                (Some(thread), Some(session)) if thread == session => Some(SourceSessionIdentity {
                    source: "codex".to_string(),
                    source_session_id: thread.to_string(),
                }),
                _ => None,
            };
        let host_identity = match (claude_identity, codex_identity) {
            (Some(identity), None) | (None, Some(identity)) => Some(identity),
            _ => None,
        };
        Self { host_identity }
    }

    fn resolve(&self, store: &Store, invocation_nonce: Option<&str>) -> CurrentSession {
        self.resolve_with_probe(
            store,
            invocation_nonce,
            adapters::invocation_probe::probe_invocation_nonce,
        )
    }

    fn resolve_with_probe<F>(
        &self,
        store: &Store,
        invocation_nonce: Option<&str>,
        probe: F,
    ) -> CurrentSession
    where
        F: FnOnce(&str) -> adapters::invocation_probe::InvocationProbeResult,
    {
        if let Some(identity) = self.host_identity.as_ref()
            && let Ok(Some(session)) =
                store.get_session_by_source_id(&identity.source, &identity.source_session_id)
        {
            return CurrentSession::resolved(&session);
        }
        let Some(invocation_nonce) = invocation_nonce.filter(|value| !value.trim().is_empty())
        else {
            return CurrentSession::unknown();
        };
        let result = probe(invocation_nonce);
        if !result.complete || result.candidates.len() != 1 {
            return CurrentSession::unknown();
        }
        let candidate = &result.candidates[0];
        store
            .get_session_by_source_id(&candidate.source, &candidate.source_id)
            .ok()
            .flatten()
            .as_ref()
            .map(CurrentSession::resolved)
            .unwrap_or_else(CurrentSession::unknown)
    }
}

fn verified_session_id(value: Option<&str>) -> Option<&str> {
    opt_trimmed(value).filter(|value| uuid::Uuid::try_parse(value).is_ok())
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
struct EventHit {
    session_id: String,
    source: String,
    project: Option<String>,
    title: String,
    timestamp: Option<String>,
    kind: String,
    name: Option<String>,
    target: Option<String>,
    event_seq: u32,
    summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
struct EventList {
    message: Option<String>,
    events: Vec<EventHit>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
struct SessionDetail {
    message: Option<String>,
    session_id: Option<String>,
    source_session_id: Option<String>,
    source: Option<String>,
    project: Option<String>,
    title: Option<String>,
    summary: Option<String>,
    timestamp: Option<String>,
    message_count: Option<u32>,
    returned_messages: usize,
    first_message_seq: Option<u32>,
    last_message_seq: Option<u32>,
    truncated: bool,
    messages: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    events: Option<Vec<SessionEventDetail>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    returned_events: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    events_truncated: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
struct SessionEventDetail {
    event_seq: u32,
    timestamp: Option<String>,
    kind: String,
    actor: String,
    name: Option<String>,
    status: Option<String>,
    target: Option<String>,
    message_seq: Option<u32>,
    source_event_id: Option<String>,
    tool_call_id: Option<String>,
    is_meta: Option<bool>,
    visibility: Option<EvidenceVisibility>,
    summary: Option<String>,
}

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
        description = "Load one indexed session by Recall session_id. Use after search_sessions or list_recent_sessions when you need the conversation itself. Each message is truncated at 2000 characters; the combined message text is capped at 32000 characters. Set include_events only when structured evidence is needed; it returns at most 50 events anchored to the returned message range plus unanchored events, caps each event string field at 200 characters and all event string fields at 10000 characters total, and never returns raw arguments, results, source paths, or parser internals. Returns metadata, including source-native source_session_id and the first_message_seq and last_message_seq represented by the response, plus messages as plain text in sequence order.",
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
        description = "Which indexed sessions touched a file. An event matches when its target equals the given path, or when either one ends with `/` or `\\` followed by the other, so relative and absolute forms of the same path find each other. Defaults to file_write and file_read events. Returns session id, source, project, title, kind, name, target, event_seq, truncated summary, and event timestamp.",
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

fn empty_events(message: impl Into<String>) -> EventList {
    EventList { message: Some(message.into()), events: Vec::new() }
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

fn search_ready(
    store: &Store,
    args: &SearchSessionsArgs,
    current_session: &CurrentSession,
) -> std::result::Result<HitList, String> {
    let (scope, sources) = resolve_filters(store, args.project.as_deref(), args.source.as_deref())?;
    let limit = clamp_limit(args.limit, SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX);
    let embedding = query_embedding(store, &args.query, |message| {
        tracing::info!("{message}");
    })
    .map_err(|error| error.to_string())?;
    let filters = SearchFilters {
        sources,
        time_range: TimeRange::All,
        scope,
        thread_role: None,
        excluded_session_id: current_session.session_id.clone(),
    };
    let results = SearchEngine::new(&store.conn)
        .hybrid_search(&args.query, embedding.as_deref(), &filters, limit, 3)
        .map_err(|error| error.to_string())?;
    if results.is_empty() {
        let message = if store_is_empty(store) { EMPTY_INDEX } else { SEARCH_EMPTY };
        return Ok(HitList {
            message: Some(message.to_string()),
            hits: Vec::new(),
            current_session: current_session.clone(),
        });
    }
    Ok(HitList {
        message: None,
        current_session: current_session.clone(),
        hits: results
            .into_iter()
            .map(|result| session_hit(&result.session, result.snippet))
            .collect(),
    })
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

fn list_ready(
    store: &Store,
    args: &ListRecentSessionsArgs,
    current_session: &CurrentSession,
) -> std::result::Result<HitList, String> {
    let (scope, sources) = resolve_filters(store, args.project.as_deref(), args.source.as_deref())?;
    let limit = clamp_limit(args.limit, LIST_LIMIT_DEFAULT, LIST_LIMIT_MAX);
    let sessions = store
        .list_recent_sessions_for_search_scope(
            sources.as_deref(),
            TimeRange::All,
            &scope,
            current_session.session_id.as_deref(),
            limit,
        )
        .map_err(|error| error.to_string())?;
    if sessions.is_empty() {
        let message = if store_is_empty(store) { EMPTY_INDEX } else { SEARCH_EMPTY };
        return Ok(HitList {
            message: Some(message.to_string()),
            hits: Vec::new(),
            current_session: current_session.clone(),
        });
    }
    Ok(HitList {
        message: None,
        current_session: current_session.clone(),
        hits: sessions
            .into_iter()
            .map(|session| {
                let excerpt =
                    session.summary.as_deref().map(|text| truncate_chars(text, EXCERPT_CHAR_CAP).0);
                session_hit(&session, excerpt)
            })
            .collect(),
    })
}

fn file_history(
    index: &IndexState,
    args: &FileHistoryArgs,
) -> std::result::Result<EventList, EventList> {
    match index {
        IndexState::Unavailable { message, .. } => Err(empty_events(message.clone())),
        IndexState::Ready(store) => file_history_ready(store, args).map_err(empty_events),
    }
}

fn file_history_ready(
    store: &Store,
    args: &FileHistoryArgs,
) -> std::result::Result<EventList, String> {
    let path = args.path.trim();
    if path.is_empty() {
        return Err(PATH_REQUIRED.to_string());
    }
    let kinds = match opt_trimmed(args.kind.as_deref()) {
        Some(kind) => vec![kind.to_string()],
        None => FILE_HISTORY_KINDS.iter().map(|kind| (*kind).to_string()).collect(),
    };
    let (scope, sources) = resolve_filters(store, args.project.as_deref(), args.source.as_deref())?;
    let hits = SearchEngine::new(&store.conn)
        .list_session_events(&SessionEventQuery {
            kinds: Some(kinds.as_slice()),
            target: path,
            sources: sources.as_deref(),
            scope: &scope,
            limit: clamp_limit(args.limit, EVENT_LIMIT_DEFAULT, EVENT_LIMIT_MAX),
        })
        .map_err(|error| error.to_string())?;
    if hits.is_empty() {
        let message = if store_is_empty(store) { EMPTY_INDEX } else { SEARCH_EMPTY };
        return Ok(EventList { message: Some(message.to_string()), events: Vec::new() });
    }
    Ok(EventList { message: None, events: hits.into_iter().map(event_hit).collect() })
}

fn opt_trimmed(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn event_hit(hit: crate::db::search::SessionEventHit) -> EventHit {
    EventHit {
        session_id: hit.session.id.clone(),
        source: hit.session.source.clone(),
        project: hit.session.directory.clone(),
        title: session_title(&hit.session),
        timestamp: hit.timestamp.map(iso8601),
        kind: hit.kind,
        name: hit.name,
        target: hit.target,
        event_seq: hit.event_seq,
        summary: hit.summary.map(|text| truncate_chars(&text, EXCERPT_CHAR_CAP).0),
    }
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

fn get_ready(store: &Store, args: &GetSessionArgs) -> std::result::Result<SessionDetail, String> {
    let Some(session) =
        store.get_session_by_id(&args.session_id).map_err(|error| error.to_string())?
    else {
        let message = if store_is_empty(store) {
            EMPTY_INDEX.to_string()
        } else {
            format!("{SESSION_NOT_FOUND} {}", args.session_id)
        };
        return Err(message);
    };
    let max_messages = clamp_limit(args.max_messages, GET_MAX_MESSAGES_DEFAULT, u32::MAX);
    let messages = store.get_messages(&session.id).map_err(|error| error.to_string())?;
    let (text, returned, truncated, first_message_seq, last_message_seq) =
        render_messages(&messages, max_messages, args.tail);
    let (events, returned_events, events_truncated) = if args.include_events {
        let records = store
            .list_session_events_for_session(&session.id)
            .map_err(|error| error.to_string())?;
        let (events, truncated) =
            render_session_events(records, first_message_seq, last_message_seq, args.tail);
        let returned = events.len();
        (Some(events), Some(returned), Some(truncated))
    } else {
        (None, None, None)
    };
    Ok(SessionDetail {
        message: None,
        session_id: Some(session.id.clone()),
        source_session_id: Some(session.source_id.clone()),
        source: Some(session.source.clone()),
        project: session.directory.clone(),
        title: Some(session_title(&session)),
        summary: session.summary.clone(),
        timestamp: Some(iso8601(session.started_at)),
        message_count: Some(session.message_count),
        returned_messages: returned,
        first_message_seq,
        last_message_seq,
        truncated,
        messages: text,
        events,
        returned_events,
        events_truncated,
    })
}

fn empty_detail(message: Option<String>) -> SessionDetail {
    SessionDetail {
        message,
        session_id: None,
        source_session_id: None,
        source: None,
        project: None,
        title: None,
        summary: None,
        timestamp: None,
        message_count: None,
        returned_messages: 0,
        first_message_seq: None,
        last_message_seq: None,
        truncated: false,
        messages: String::new(),
        events: None,
        returned_events: None,
        events_truncated: None,
    }
}

fn render_session_events(
    records: Vec<SessionEventRecord>,
    first_message_seq: Option<u32>,
    last_message_seq: Option<u32>,
    tail: bool,
) -> (Vec<SessionEventDetail>, bool) {
    let total_records = records.len();
    let filtered = records
        .iter()
        .filter(|event| match event.message_seq {
            None => true,
            Some(message_seq) => first_message_seq
                .zip(last_message_seq)
                .is_some_and(|(first, last)| message_seq >= first && message_seq <= last),
        })
        .collect::<Vec<_>>();
    let mut truncated = filtered.len() != total_records;
    let selected = if tail {
        filtered.iter().rev().take(GET_EVENT_LIMIT).copied().collect::<Vec<_>>()
    } else {
        filtered.iter().take(GET_EVENT_LIMIT).copied().collect::<Vec<_>>()
    };
    if selected.len() != filtered.len() {
        truncated = true;
    }

    let mut details = Vec::new();
    let mut text_chars = 0;
    for record in selected {
        let (detail, fields_truncated) = session_event_detail(record);
        let detail_chars = session_event_text_chars(&detail);
        if text_chars + detail_chars > GET_EVENT_TEXT_CHAR_CAP {
            truncated = true;
            break;
        }
        text_chars += detail_chars;
        truncated |= fields_truncated;
        details.push(detail);
    }
    details.sort_by_key(|event| match event.message_seq {
        Some(message_seq) => (0_u8, message_seq, event.event_seq),
        None => (1_u8, 0, event.event_seq),
    });
    (details, truncated)
}

fn session_event_detail(record: &SessionEventRecord) -> (SessionEventDetail, bool) {
    let (kind, kind_truncated) = truncate_chars(&record.kind, GET_EVENT_FIELD_CHAR_CAP);
    let (actor, actor_truncated) = truncate_chars(&record.actor, GET_EVENT_FIELD_CHAR_CAP);
    let (name, name_truncated) = truncate_event_field(record.name.as_deref());
    let (status, status_truncated) = truncate_event_field(record.status.as_deref());
    let (target, target_truncated) = truncate_event_field(record.target.as_deref());
    let (source_event_id, source_event_id_truncated) =
        truncate_event_field(record.source_event_id.as_deref());
    let (tool_call_id, tool_call_id_truncated) =
        truncate_event_field(record.tool_call_id.as_deref());
    let (summary, summary_truncated) = truncate_event_field(record.summary.as_deref());
    (
        SessionEventDetail {
            event_seq: record.event_seq,
            timestamp: record.timestamp.map(iso8601),
            kind,
            actor,
            name,
            status,
            target,
            message_seq: record.message_seq,
            source_event_id,
            tool_call_id,
            is_meta: record.is_meta,
            visibility: record.visibility,
            summary,
        },
        kind_truncated
            || actor_truncated
            || name_truncated
            || status_truncated
            || target_truncated
            || source_event_id_truncated
            || tool_call_id_truncated
            || summary_truncated,
    )
}

fn truncate_event_field(value: Option<&str>) -> (Option<String>, bool) {
    match value {
        Some(value) => {
            let (value, truncated) = truncate_chars(value, GET_EVENT_FIELD_CHAR_CAP);
            (Some(value), truncated)
        }
        None => (None, false),
    }
}

fn session_event_text_chars(event: &SessionEventDetail) -> usize {
    event.kind.chars().count()
        + event.actor.chars().count()
        + optional_text_chars(event.timestamp.as_deref())
        + optional_text_chars(event.name.as_deref())
        + optional_text_chars(event.status.as_deref())
        + optional_text_chars(event.target.as_deref())
        + optional_text_chars(event.source_event_id.as_deref())
        + optional_text_chars(event.tool_call_id.as_deref())
        + event.visibility.map(|value| value.as_str().chars().count()).unwrap_or(0)
        + optional_text_chars(event.summary.as_deref())
}

fn optional_text_chars(value: Option<&str>) -> usize {
    value.map(|value| value.chars().count()).unwrap_or(0)
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

fn render_messages(
    messages: &[Message],
    max_messages: usize,
    tail: bool,
) -> (String, usize, bool, Option<u32>, Option<u32>) {
    let selected = if tail {
        messages.iter().rev().take(max_messages).collect::<Vec<_>>()
    } else {
        messages.iter().take(max_messages).collect::<Vec<_>>()
    };
    let mut blocks = Vec::new();
    let mut bytes = 0;
    let mut truncated = messages.len() > selected.len();
    for message in selected {
        let (body, cut) = truncate_chars(&message.content, GET_MESSAGE_CHAR_CAP);
        if cut {
            truncated = true;
        }
        let block = format!("[{}] {body}", role_label(&message.role));
        let extra = if blocks.is_empty() { block.len() } else { block.len() + 2 };
        if bytes + extra > GET_RESPONSE_CHAR_CAP {
            truncated = true;
            break;
        }
        bytes += extra;
        blocks.push((message.seq, block));
    }
    if tail {
        blocks.reverse();
    }
    let returned = blocks.len();
    let first_message_seq = blocks.first().map(|(seq, _)| *seq);
    let last_message_seq = blocks.last().map(|(seq, _)| *seq);
    let text = blocks.into_iter().map(|(_, block)| block).collect::<Vec<_>>().join("\n\n");
    (text, returned, truncated, first_message_seq, last_message_seq)
}

fn role_label(role: &Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::db::store::SessionTopologyWrite;
    use crate::types::{ParentLink, ParentRelation, RawSessionEvent, Session, ThreadRole};

    fn setup() -> Store {
        schema::register_sqlite_vec();
        Store::open_in_memory().unwrap()
    }

    fn session(id: &str, source: &str, title: &str, started_at: i64) -> Session {
        Session {
            id: id.to_string(),
            source: source.to_string(),
            source_id: format!("src-{id}"),
            title: title.to_string(),
            directory: Some("/tmp/demo".to_string()),
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
            started_at,
            updated_at: Some(started_at),
            message_count: 1,
            entrypoint: None,
            custom_title: None,
            summary: Some(format!("{title} summary")),
            duration_minutes: None,
            source_file_path: None,
            is_import: false,
        }
    }

    fn message(session_id: &str, role: Role, content: &str, seq: u32) -> Message {
        Message {
            session_id: session_id.to_string(),
            role,
            content: content.to_string(),
            timestamp: Some(1_700_000_000_000),
            seq,
        }
    }

    fn ready(store: Store) -> IndexState {
        IndexState::Ready(store)
    }

    fn context(source: &str, source_session_id: &str) -> CurrentSessionContext {
        CurrentSessionContext {
            host_identity: Some(SourceSessionIdentity {
                source: source.to_string(),
                source_session_id: source_session_id.to_string(),
            }),
        }
    }

    fn probe_result(
        candidates: &[(&str, &str)],
        complete: bool,
    ) -> adapters::invocation_probe::InvocationProbeResult {
        adapters::invocation_probe::InvocationProbeResult {
            candidates: candidates
                .iter()
                .map(|(source, source_id)| adapters::invocation_probe::InvocationProbeCandidate {
                    source: (*source).to_string(),
                    source_id: (*source_id).to_string(),
                })
                .collect(),
            complete,
        }
    }

    fn without_fields(value: impl Serialize, fields: &[&str]) -> Value {
        let mut value = to_json(value);
        let object = value.as_object_mut().unwrap();
        for field in fields {
            object.remove(*field);
        }
        value
    }

    #[test]
    fn missing_index_is_a_tool_error() {
        let index = IndexState::Unavailable { path: None, message: MISSING_INDEX.to_string() };
        let args = SearchSessionsArgs {
            query: "error".into(),
            project: None,
            source: None,
            limit: None,
            invocation_nonce: None,
        };
        let list = search_sessions(&index, &args).expect_err("missing index");
        assert!(list.hits.is_empty());
        assert_eq!(list.message.as_deref(), Some(MISSING_INDEX));
        assert_eq!(json_result(search_sessions(&index, &args)).is_error, Some(true));
    }

    #[test]
    fn empty_index_search_and_list_explain_the_gap() {
        let store = setup();
        let index = ready(store);
        let search_args = SearchSessionsArgs {
            query: "anything".into(),
            project: None,
            source: None,
            limit: None,
            invocation_nonce: None,
        };
        let search = search_sessions(&index, &search_args).unwrap();
        assert!(search.hits.is_empty());
        assert_eq!(search.message.as_deref(), Some(EMPTY_INDEX));
        assert_eq!(json_result(search_sessions(&index, &search_args)).is_error, Some(false));

        let list_args = ListRecentSessionsArgs {
            project: None,
            source: None,
            limit: None,
            invocation_nonce: None,
        };
        let listed = list_recent_sessions(&index, &list_args).unwrap();
        assert!(listed.hits.is_empty());
        assert_eq!(listed.message.as_deref(), Some(EMPTY_INDEX));
        assert_eq!(json_result(list_recent_sessions(&index, &list_args)).is_error, Some(false));

        let history = file_history(&index, &file_history_args("src/db/schema.rs")).unwrap();
        assert!(history.events.is_empty());
        assert_eq!(history.message.as_deref(), Some(EMPTY_INDEX));
        assert_eq!(
            json_result(file_history(&index, &file_history_args("src/db/schema.rs"))).is_error,
            Some(false)
        );
    }

    #[test]
    fn search_reuses_hybrid_search_and_clamps_limit() {
        let store = setup();
        store.insert_session(&session("s1", "codex", "iterator panic", 2_000)).unwrap();
        store
            .insert_messages(&[message("s1", Role::User, "how do I use iterators in Rust", 0)])
            .unwrap();
        let index = ready(store);

        let list = search_sessions(
            &index,
            &SearchSessionsArgs {
                query: "iterators".into(),
                project: None,
                source: None,
                limit: Some(80),
                invocation_nonce: None,
            },
        )
        .unwrap();
        assert_eq!(list.hits.len(), 1);
        assert_eq!(list.hits[0].session_id, "s1");
        assert_eq!(list.hits[0].source_session_id, "src-s1");
        assert_eq!(list.hits[0].source, "codex");
        assert_eq!(list.hits[0].project.as_deref(), Some("/tmp/demo"));
        assert_eq!(list.hits[0].title, "iterator panic");
        assert!(list.hits[0].excerpt.as_deref().unwrap().contains("iterators"));
        assert_eq!(list.hits[0].timestamp, iso8601(2_000));
        assert!(list.message.is_none());
        assert_eq!(
            without_fields(&list.hits[0], &["source_session_id"]),
            serde_json::json!({
                "session_id": "s1",
                "source": "codex",
                "project": "/tmp/demo",
                "title": "iterator panic",
                "excerpt": "how do I use iterators in Rust",
                "timestamp": iso8601(2_000),
            })
        );
    }

    #[test]
    fn list_recent_is_newest_first_and_matches_search_shape() {
        let store = setup();
        store.insert_session(&session("old", "codex", "older", 1_000)).unwrap();
        store.insert_session(&session("new", "claude-code", "newer", 9_000)).unwrap();
        let index = ready(store);

        let list = list_recent_sessions(
            &index,
            &ListRecentSessionsArgs {
                project: None,
                source: None,
                limit: Some(10),
                invocation_nonce: None,
            },
        )
        .unwrap();
        assert_eq!(list.hits.len(), 2);
        assert_eq!(list.hits[0].session_id, "new");
        assert_eq!(list.hits[0].source_session_id, "src-new");
        assert_eq!(list.hits[0].excerpt.as_deref(), Some("newer summary"));
        assert_eq!(list.hits[1].session_id, "old");
        assert_eq!(list.hits[1].source_session_id, "src-old");
    }

    #[test]
    fn host_identity_requires_verified_source_native_values() {
        let codex_id = "019c9c4f-a462-7cc1-99a5-4ab521648c91";
        let codex = CurrentSessionContext::from_values(None, Some(codex_id), Some(codex_id));
        assert_eq!(
            codex.host_identity,
            Some(SourceSessionIdentity {
                source: "codex".to_string(),
                source_session_id: codex_id.to_string(),
            })
        );
        assert!(
            CurrentSessionContext::from_values(None, Some("thread"), Some("session"))
                .host_identity
                .is_none()
        );
        assert!(
            CurrentSessionContext::from_values(None, Some(codex_id), None).host_identity.is_none()
        );
        assert!(
            CurrentSessionContext::from_values(None, Some(" "), Some(" ")).host_identity.is_none()
        );
        assert!(
            CurrentSessionContext::from_values(None, Some("same"), Some("same"))
                .host_identity
                .is_none()
        );
        let claude_id = "604c4e71-f49c-4cc0-9388-88905fe65473";
        let claude = CurrentSessionContext::from_values(Some(claude_id), None, None);
        assert_eq!(
            claude.host_identity,
            Some(SourceSessionIdentity {
                source: "claude-code".to_string(),
                source_session_id: claude_id.to_string(),
            })
        );
        assert!(
            CurrentSessionContext::from_values(Some(claude_id), Some(codex_id), Some(codex_id))
                .host_identity
                .is_none()
        );
    }

    #[test]
    fn resolved_current_session_is_excluded_before_search_and_recent_limits() {
        let store = setup();
        let mut current = session("000-current", "codex", "current", 100_000);
        current.message_count = 1;
        store.insert_session(&current).unwrap();
        store.insert_messages(&[message("000-current", Role::User, "identityneedle", 0)]).unwrap();
        for index in 0..51 {
            let id = format!("history-{index:02}");
            let mut stored = session(&id, "codex", "history", 50_000 - index);
            stored.message_count = 1;
            store.insert_session(&stored).unwrap();
            store.insert_messages(&[message(&id, Role::User, "identityneedle", 0)]).unwrap();
        }
        store
            .persist_topology_for_existing_session(
                "codex",
                "src-history-00",
                &SessionTopologyWrite {
                    thread_role: Some(ThreadRole::Primary),
                    parents: &[],
                    parser_version: Some(1),
                },
            )
            .unwrap();
        let spawn = ParentLink {
            relation: ParentRelation::Spawn,
            source: "codex".to_string(),
            source_id: "src-history-00".to_string(),
        };
        store
            .persist_topology_for_existing_session(
                "codex",
                "src-history-01",
                &SessionTopologyWrite {
                    thread_role: Some(ThreadRole::Subagent),
                    parents: std::slice::from_ref(&spawn),
                    parser_version: Some(1),
                },
            )
            .unwrap();
        let index = ready(store);
        let current_context = context("codex", "src-000-current");

        let first = search_sessions_with_context(
            &index,
            &SearchSessionsArgs {
                query: "identityneedle".into(),
                project: None,
                source: None,
                limit: Some(1),
                invocation_nonce: None,
            },
            &current_context,
        )
        .unwrap();
        assert_eq!(first.hits.len(), 1);
        assert_ne!(first.hits[0].session_id, "000-current");
        assert_eq!(first.current_session.resolution, CurrentSessionResolution::Resolved);
        assert_eq!(first.current_session.session_id.as_deref(), Some("000-current"));

        let fifty = search_sessions_with_context(
            &index,
            &SearchSessionsArgs {
                query: "identityneedle".into(),
                project: None,
                source: None,
                limit: Some(50),
                invocation_nonce: None,
            },
            &current_context,
        )
        .unwrap();
        assert_eq!(fifty.hits.len(), 50);
        assert!(fifty.hits.iter().all(|hit| hit.session_id != "000-current"));

        let recent = list_recent_sessions_with_context(
            &index,
            &ListRecentSessionsArgs {
                project: None,
                source: None,
                limit: Some(1),
                invocation_nonce: None,
            },
            &current_context,
        )
        .unwrap();
        assert_eq!(recent.hits.len(), 1);
        assert_ne!(recent.hits[0].session_id, "000-current");
        assert_eq!(recent.current_session.resolution, CurrentSessionResolution::Resolved);

        let recent_fifty = list_recent_sessions_with_context(
            &index,
            &ListRecentSessionsArgs {
                project: None,
                source: None,
                limit: Some(50),
                invocation_nonce: None,
            },
            &current_context,
        )
        .unwrap();
        assert_eq!(recent_fifty.hits.len(), 50);
        assert!(recent_fifty.hits.iter().all(|hit| hit.session_id != "000-current"));
    }

    #[test]
    fn unresolved_or_mismatched_host_identity_keeps_discovery_complete() {
        let store = setup();
        let mut stored = session("current", "claude-code", "current", 10_000);
        stored.source_id = "shared-source-id".to_string();
        store.insert_session(&stored).unwrap();
        store.insert_messages(&[message("current", Role::User, "mismatchneedle", 0)]).unwrap();
        let index = ready(store);

        for current_context in [
            context("codex", "shared-source-id"),
            context("codex", "not-indexed"),
            CurrentSessionContext::from_values(None, Some("a"), Some("b")),
        ] {
            let result = search_sessions_with_context(
                &index,
                &SearchSessionsArgs {
                    query: "mismatchneedle".into(),
                    project: None,
                    source: None,
                    limit: Some(1),
                    invocation_nonce: None,
                },
                &current_context,
            )
            .unwrap();
            assert_eq!(result.current_session, CurrentSession::unknown());
            assert_eq!(result.hits[0].session_id, "current");
        }
    }

    #[test]
    fn discovery_scope_does_not_change_exact_current_session_resolution() {
        let store = setup();
        let current = session("current", "codex", "current", 10_000);
        store.insert_session(&current).unwrap();
        let mut other = session("other", "claude-code", "other", 9_000);
        other.directory = Some("/tmp/other".to_string());
        store.insert_session(&other).unwrap();
        store.insert_messages(&[message("other", Role::User, "scopeneedle", 0)]).unwrap();
        let result = search_sessions_with_context(
            &ready(store),
            &SearchSessionsArgs {
                query: "scopeneedle".into(),
                project: Some("/tmp/other".into()),
                source: Some("claude-code".into()),
                limit: Some(1),
                invocation_nonce: None,
            },
            &context("codex", "src-current"),
        )
        .unwrap();
        assert_eq!(result.current_session.resolution, CurrentSessionResolution::Resolved);
        assert_eq!(result.hits[0].session_id, "other");
    }

    #[test]
    fn exact_reads_remain_complete_for_the_resolved_current_session() {
        let store = setup();
        let mut current = session("current", "codex", "current", 10_000);
        current.message_count = 1;
        store.insert_session(&current).unwrap();
        store.insert_messages(&[message("current", Role::User, "exact transcript", 0)]).unwrap();
        persist_events(
            &store,
            "codex",
            "current",
            &[event(0, "file_write", Some("src/current.rs"), Some(10_000), None, None)],
        );
        let index = ready(store);

        let detail = get_session(
            &index,
            &GetSessionArgs {
                session_id: "current".into(),
                max_messages: None,
                tail: false,
                include_events: false,
            },
        )
        .unwrap();
        assert_eq!(detail.messages, "[user] exact transcript");
        let history = file_history(&index, &file_history_args("src/current.rs")).unwrap();
        assert_eq!(history.events.len(), 1);
        assert_eq!(history.events[0].session_id, "current");
    }

    #[test]
    fn invocation_nonce_resolves_only_one_complete_indexed_candidate() {
        let store = setup();
        let mut current = session("current", "codex", "current", 10_000);
        current.source_id = "nonce-source".to_string();
        store.insert_session(&current).unwrap();
        let context = CurrentSessionContext::default();

        let resolved = context.resolve_with_probe(&store, Some("nonce"), |_| {
            probe_result(&[("codex", "nonce-source")], true)
        });
        assert_eq!(resolved.resolution, CurrentSessionResolution::Resolved);
        assert_eq!(resolved.session_id.as_deref(), Some("current"));

        for result in [
            probe_result(&[], true),
            probe_result(&[("codex", "nonce-source"), ("claude-code", "other")], true),
            probe_result(&[("codex", "nonce-source")], false),
            probe_result(&[("codex", "unindexed")], true),
            probe_result(&[("claude-code", "nonce-source")], true),
        ] {
            assert_eq!(
                context.resolve_with_probe(&store, Some("nonce"), |_| result),
                CurrentSession::unknown()
            );
        }
        assert_eq!(
            context.resolve_with_probe(&store, Some(" "), |_| {
                panic!("blank nonce must not probe")
            }),
            CurrentSession::unknown()
        );
    }

    #[test]
    fn resolved_host_identity_skips_invocation_probe() {
        let store = setup();
        let mut current = session("current", "codex", "current", 10_000);
        current.source_id = "host-source".to_string();
        store.insert_session(&current).unwrap();

        let resolved =
            context("codex", "host-source").resolve_with_probe(&store, Some("nonce"), |_| {
                panic!("resolved host identity must skip probing")
            });
        assert_eq!(resolved.resolution, CurrentSessionResolution::Resolved);
        assert_eq!(resolved.session_id.as_deref(), Some("current"));
    }

    #[test]
    fn unknown_source_is_empty_not_a_crash() {
        let store = setup();
        store.insert_session(&session("s1", "codex", "title", 1_000)).unwrap();
        store.insert_messages(&[message("s1", Role::User, "hello", 0)]).unwrap();
        let index = ready(store);
        let args = SearchSessionsArgs {
            query: "hello".into(),
            project: None,
            source: Some("not-a-source".into()),
            limit: None,
            invocation_nonce: None,
        };
        let list = search_sessions(&index, &args).expect_err("unknown source");
        assert!(list.hits.is_empty());
        assert!(list.message.unwrap().contains("unknown source"));
        assert_eq!(json_result(search_sessions(&index, &args)).is_error, Some(true));
    }

    #[test]
    fn get_session_returns_plain_text_and_caps_long_messages() {
        let store = setup();
        let mut long = session("s1", "codex", "long", 1_000);
        long.message_count = 2;
        store.insert_session(&long).unwrap();
        let huge = "x".repeat(GET_MESSAGE_CHAR_CAP + 20);
        store
            .insert_messages(&[
                message("s1", Role::User, &huge, 0),
                message("s1", Role::Assistant, "done", 1),
            ])
            .unwrap();
        let index = ready(store);

        let detail = get_session(
            &index,
            &GetSessionArgs {
                session_id: "s1".into(),
                max_messages: Some(1),
                tail: false,
                include_events: false,
            },
        )
        .unwrap();
        assert_eq!(detail.session_id.as_deref(), Some("s1"));
        assert_eq!(detail.source_session_id.as_deref(), Some("src-s1"));
        assert_eq!(detail.returned_messages, 1);
        assert_eq!(detail.first_message_seq, Some(0));
        assert_eq!(detail.last_message_seq, Some(0));
        assert!(detail.truncated);
        assert!(detail.messages.starts_with("[user] "));
        assert!(detail.messages.ends_with('…'));
        assert!(!detail.messages.contains("[assistant]"));
        assert!(
            detail.messages.chars().count() <= "[user] ".chars().count() + GET_MESSAGE_CHAR_CAP
        );
    }

    #[test]
    fn get_session_adds_provenance_without_changing_legacy_fields() {
        let store = setup();
        let mut stored = session("s1", "codex", "ordinary", 1_000);
        stored.message_count = 2;
        store.insert_session(&stored).unwrap();
        store
            .insert_messages(&[
                message("s1", Role::User, "question", 4),
                message("s1", Role::Assistant, "answer", 7),
            ])
            .unwrap();
        let detail = get_session(
            &ready(store),
            &GetSessionArgs {
                session_id: "s1".into(),
                max_messages: None,
                tail: false,
                include_events: false,
            },
        )
        .unwrap();

        assert_eq!(detail.source_session_id.as_deref(), Some("src-s1"));
        assert_eq!(detail.first_message_seq, Some(4));
        assert_eq!(detail.last_message_seq, Some(7));
        assert_eq!(
            without_fields(
                &detail,
                &["source_session_id", "first_message_seq", "last_message_seq"]
            ),
            serde_json::json!({
                "message": null,
                "session_id": "s1",
                "source": "codex",
                "project": "/tmp/demo",
                "title": "ordinary",
                "summary": "ordinary summary",
                "timestamp": iso8601(1_000),
                "message_count": 2,
                "returned_messages": 2,
                "truncated": false,
                "messages": "[user] question\n\n[assistant] answer",
            })
        );
    }

    #[test]
    fn get_session_empty_messages_have_null_sequence_range() {
        let store = setup();
        let mut stored = session("s1", "codex", "empty", 1_000);
        stored.message_count = 0;
        store.insert_session(&stored).unwrap();
        let detail = get_session(
            &ready(store),
            &GetSessionArgs {
                session_id: "s1".into(),
                max_messages: None,
                tail: false,
                include_events: false,
            },
        )
        .unwrap();

        assert_eq!(detail.source_session_id.as_deref(), Some("src-s1"));
        assert_eq!(detail.returned_messages, 0);
        assert_eq!(detail.first_message_seq, None);
        assert_eq!(detail.last_message_seq, None);
        assert!(!detail.truncated);
        assert!(detail.messages.is_empty());
    }

    #[test]
    fn get_session_tail_returns_latest_messages_in_sequence_order() {
        let store = setup();
        let mut stored = session("s1", "codex", "unfinished", 1_000);
        stored.message_count = 4;
        store.insert_session(&stored).unwrap();
        store
            .insert_messages(&[
                message("s1", Role::User, "first", 0),
                message("s1", Role::Assistant, "second", 1),
                message("s1", Role::User, "third", 2),
                message("s1", Role::Assistant, "fourth", 3),
            ])
            .unwrap();
        let index = ready(store);

        let detail = get_session(
            &index,
            &GetSessionArgs {
                session_id: "s1".into(),
                max_messages: Some(2),
                tail: true,
                include_events: false,
            },
        )
        .unwrap();

        assert_eq!(detail.returned_messages, 2);
        assert_eq!(detail.first_message_seq, Some(2));
        assert_eq!(detail.last_message_seq, Some(3));
        assert!(detail.truncated);
        assert_eq!(detail.messages, "[user] third\n\n[assistant] fourth");
    }

    #[test]
    fn get_session_tail_keeps_newest_messages_under_response_cap() {
        let store = setup();
        let mut stored = session("s1", "codex", "long tail", 1_000);
        stored.message_count = 20;
        store.insert_session(&stored).unwrap();
        let messages = (0..20)
            .map(|seq| {
                message(
                    "s1",
                    Role::Assistant,
                    &format!("message-{seq:02}-{}", "x".repeat(1_990)),
                    seq,
                )
            })
            .collect::<Vec<_>>();
        store.insert_messages(&messages).unwrap();
        let index = ready(store);

        let detail = get_session(
            &index,
            &GetSessionArgs {
                session_id: "s1".into(),
                max_messages: Some(20),
                tail: true,
                include_events: false,
            },
        )
        .unwrap();

        assert!(detail.truncated);
        assert_eq!(detail.returned_messages, 15);
        assert_eq!(detail.first_message_seq, Some(5));
        assert_eq!(detail.last_message_seq, Some(19));
        assert!(!detail.messages.contains("message-04-"));
        assert!(detail.messages.contains("message-05-"));
        assert!(
            detail.messages.rsplit("\n\n").next().unwrap().starts_with("[assistant] message-19-")
        );
    }

    #[test]
    fn get_session_without_events_preserves_json_bytes_and_skips_event_read() {
        let store = setup();
        store.insert_session(&session("s1", "codex", "title", 1_000)).unwrap();
        store.insert_messages(&[message("s1", Role::User, "hello", 0)]).unwrap();
        store.conn.execute_batch("DROP TABLE session_events").unwrap();
        let args = GetSessionArgs {
            session_id: "s1".into(),
            max_messages: None,
            tail: false,
            include_events: false,
        };

        let index = ready(store);
        let detail = get_session(&index, &args).unwrap();
        let bytes = serde_json::to_vec(&detail).unwrap();

        assert_eq!(
            bytes,
            br#"{"message":null,"session_id":"s1","source_session_id":"src-s1","source":"codex","project":"/tmp/demo","title":"title","summary":"title summary","timestamp":"1970-01-01T00:00:01.000Z","message_count":1,"returned_messages":1,"first_message_seq":0,"last_message_seq":0,"truncated":false,"messages":"[user] hello"}"#
        );
        assert!(detail.events.is_none());
        assert!(detail.returned_events.is_none());
        assert!(detail.events_truncated.is_none());
        let event_error = get_session(&index, &GetSessionArgs { include_events: true, ..args })
            .expect_err("include_events must read the event table");
        assert!(event_error.contains("session_events"));
    }

    #[test]
    fn get_session_events_follow_returned_message_range_and_preserve_null_anchors() {
        let store = setup();
        let mut stored = session("s1", "codex", "events", 1_000);
        stored.message_count = 4;
        store.insert_session(&stored).unwrap();
        store
            .insert_messages(&[
                message("s1", Role::User, "zero", 0),
                message("s1", Role::Assistant, "one", 1),
                message("s1", Role::User, "two", 2),
                message("s1", Role::Assistant, "three", 3),
            ])
            .unwrap();
        let mut events = (0..5)
            .map(|seq| event(seq, "tool_call", None, Some(1_700_000_000_000), None, None))
            .collect::<Vec<_>>();
        events[0].message_seq = Some(0);
        events[1].message_seq = None;
        events[2].message_seq = Some(2);
        events[2].source_event_id = Some("line:2".to_string());
        events[2].tool_call_id = Some("call-2".to_string());
        events[2].is_meta = Some(false);
        events[2].visibility = Some(EvidenceVisibility::Visible);
        events[3].message_seq = Some(3);
        events[4].message_seq = None;
        persist_events(&store, "codex", "s1", &events);
        let index = ready(store);

        let head = get_session(
            &index,
            &GetSessionArgs {
                session_id: "s1".into(),
                max_messages: Some(2),
                tail: false,
                include_events: true,
            },
        )
        .unwrap();
        assert_eq!(
            head.events.as_ref().unwrap().iter().map(|event| event.event_seq).collect::<Vec<_>>(),
            vec![0, 1, 4]
        );
        assert_eq!(head.returned_events, Some(3));
        assert_eq!(head.events_truncated, Some(true));

        let tail = get_session(
            &index,
            &GetSessionArgs {
                session_id: "s1".into(),
                max_messages: Some(2),
                tail: true,
                include_events: true,
            },
        )
        .unwrap();
        let returned = tail.events.as_ref().unwrap();
        assert_eq!(
            returned.iter().map(|event| event.event_seq).collect::<Vec<_>>(),
            vec![2, 3, 1, 4]
        );
        assert_eq!(returned[0].timestamp.as_deref(), Some("2023-11-14T22:13:20.000Z"));
        assert_eq!(returned[0].source_event_id.as_deref(), Some("line:2"));
        assert_eq!(returned[0].tool_call_id.as_deref(), Some("call-2"));
        assert_eq!(returned[0].is_meta, Some(false));
        assert_eq!(returned[0].visibility, Some(EvidenceVisibility::Visible));
        assert_eq!(tail.returned_events, Some(4));
        assert_eq!(tail.events_truncated, Some(true));
        let value = serde_json::to_value(&tail).unwrap();
        let serialized = value.to_string();
        assert!(!serialized.contains("attrs_json"));
        assert!(!serialized.contains("source_path"));
        assert!(!serialized.contains("parser_version"));
        assert!(!serialized.contains("secret-value"));
    }

    #[test]
    fn get_session_events_apply_head_and_tail_event_caps() {
        let store = setup();
        let mut stored = session("s1", "codex", "event cap", 1_000);
        stored.message_count = 0;
        store.insert_session(&stored).unwrap();
        let events = (0..55)
            .map(|seq| {
                let mut event = event(seq, "tool_call", None, None, None, None);
                event.message_seq = None;
                event
            })
            .collect::<Vec<_>>();
        persist_events(&store, "codex", "s1", &events);
        let index = ready(store);

        for (tail, first, last) in [(false, 0, 49), (true, 5, 54)] {
            let detail = get_session(
                &index,
                &GetSessionArgs {
                    session_id: "s1".into(),
                    max_messages: None,
                    tail,
                    include_events: true,
                },
            )
            .unwrap();
            let events = detail.events.unwrap();
            assert_eq!(events.len(), GET_EVENT_LIMIT);
            assert_eq!(events.first().unwrap().event_seq, first);
            assert_eq!(events.last().unwrap().event_seq, last);
            assert_eq!(detail.returned_events, Some(GET_EVENT_LIMIT));
            assert_eq!(detail.events_truncated, Some(true));
        }
    }

    #[test]
    fn get_session_events_bound_unicode_fields_and_total_text() {
        let store = setup();
        let mut stored = session("s1", "codex", "event text cap", 1_000);
        stored.message_count = 0;
        store.insert_session(&stored).unwrap();
        let long = "汉".repeat(GET_EVENT_FIELD_CHAR_CAP + 50);
        let events = (0..GET_EVENT_LIMIT as u32)
            .map(|seq| {
                let mut event = event(seq, "tool_call", None, None, Some(&long), None);
                event.message_seq = None;
                event.target = Some(long.clone());
                event
            })
            .collect::<Vec<_>>();
        persist_events(&store, "codex", "s1", &events);

        let detail = get_session(
            &ready(store),
            &GetSessionArgs {
                session_id: "s1".into(),
                max_messages: None,
                tail: false,
                include_events: true,
            },
        )
        .unwrap();
        let events = detail.events.unwrap();
        assert!(events.len() < GET_EVENT_LIMIT);
        assert_eq!(detail.returned_events, Some(events.len()));
        assert_eq!(detail.events_truncated, Some(true));
        assert!(events.iter().all(|event| {
            let summary = event.summary.as_deref().unwrap();
            summary.chars().count() == GET_EVENT_FIELD_CHAR_CAP && summary.ends_with('…')
        }));
        assert!(
            events.iter().map(session_event_text_chars).sum::<usize>() <= GET_EVENT_TEXT_CHAR_CAP
        );
    }

    #[test]
    fn get_session_include_events_returns_explicit_empty_payload() {
        let store = setup();
        let mut stored = session("s1", "codex", "no events", 1_000);
        stored.message_count = 0;
        store.insert_session(&stored).unwrap();

        let detail = get_session(
            &ready(store),
            &GetSessionArgs {
                session_id: "s1".into(),
                max_messages: None,
                tail: false,
                include_events: true,
            },
        )
        .unwrap();

        assert_eq!(detail.events, Some(Vec::new()));
        assert_eq!(detail.returned_events, Some(0));
        assert_eq!(detail.events_truncated, Some(false));
    }

    #[test]
    fn get_session_missing_id_is_a_tool_error() {
        let store = setup();
        store.insert_session(&session("s1", "codex", "title", 1_000)).unwrap();
        let index = ready(store);
        let args = GetSessionArgs {
            session_id: "missing".into(),
            max_messages: None,
            tail: false,
            include_events: false,
        };
        let error = get_session(&index, &args).expect_err("missing session");
        assert!(error.contains(SESSION_NOT_FOUND));
        assert_eq!(
            json_result(get_session(&index, &args).map_err(|message| empty_detail(Some(message))))
                .is_error,
            Some(true)
        );
    }

    #[test]
    fn open_read_only_rejects_writes() {
        schema::register_sqlite_vec();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recall.db");
        {
            let store = Store::open_at(&path).unwrap();
            store.insert_session(&session("s1", "codex", "title", 1_000)).unwrap();
        }
        let store = Store::open_read_only_at(&path).unwrap();
        let loaded = store.get_session_by_id("s1").unwrap().unwrap();
        assert_eq!(loaded.title, "title");
        assert!(store.insert_session(&session("s2", "codex", "nope", 2_000)).is_err());
    }

    #[test]
    fn missing_db_path_does_not_create_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("recall.db");
        let index = IndexState::open(Some(&path));
        match index {
            IndexState::Unavailable { message, path: kept } => {
                assert!(message.contains("not found"));
                assert_eq!(kept.as_deref(), Some(path.as_path()));
                assert!(!path.exists());
            }
            IndexState::Ready(_) => panic!("missing database should stay unavailable"),
        }
    }

    #[test]
    fn missing_index_opens_after_the_file_appears() {
        schema::register_sqlite_vec();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recall.db");
        let mut index = IndexState::open(Some(&path));
        match &index {
            IndexState::Unavailable { message, path: kept } => {
                assert!(message.contains("not found"));
                assert_eq!(kept.as_deref(), Some(path.as_path()));
            }
            IndexState::Ready(_) => panic!("missing database should stay unavailable"),
        }
        {
            let store = Store::open_at(&path).unwrap();
            store.insert_session(&session("s1", "codex", "later", 1_000)).unwrap();
            store.insert_messages(&[message("s1", Role::User, "hello later", 0)]).unwrap();
        }
        index.ensure_open();
        let listed = list_recent_sessions(
            &index,
            &ListRecentSessionsArgs {
                project: None,
                source: None,
                limit: None,
                invocation_nonce: None,
            },
        )
        .unwrap();
        assert_eq!(listed.hits.len(), 1);
        assert_eq!(listed.hits[0].session_id, "s1");
        assert!(listed.message.is_none());
    }

    #[test]
    fn list_recent_bounds_summary_excerpt() {
        let store = setup();
        let mut long = session("s1", "codex", "long", 1_000);
        long.summary = Some("y".repeat(EXCERPT_CHAR_CAP + 40));
        store.insert_session(&long).unwrap();
        let index = ready(store);
        let list = list_recent_sessions(
            &index,
            &ListRecentSessionsArgs {
                project: None,
                source: None,
                limit: None,
                invocation_nonce: None,
            },
        )
        .unwrap();
        let excerpt = list.hits[0].excerpt.as_deref().unwrap();
        assert_eq!(excerpt.chars().count(), EXCERPT_CHAR_CAP);
        assert!(excerpt.ends_with('…'));
    }

    #[test]
    fn get_session_flags_truncated_when_one_char_over_cap() {
        let store = setup();
        let mut long = session("s1", "codex", "edge", 1_000);
        long.message_count = 1;
        store.insert_session(&long).unwrap();
        let content = "x".repeat(GET_MESSAGE_CHAR_CAP + 1);
        store.insert_messages(&[message("s1", Role::User, &content, 0)]).unwrap();
        let index = ready(store);
        let detail = get_session(
            &index,
            &GetSessionArgs {
                session_id: "s1".into(),
                max_messages: None,
                tail: false,
                include_events: false,
            },
        )
        .unwrap();
        assert!(detail.truncated);
        assert_eq!(detail.first_message_seq, Some(0));
        assert_eq!(detail.last_message_seq, Some(0));
        assert!(detail.messages.ends_with('…'));
        assert_eq!(
            detail.messages.chars().count(),
            "[user] ".chars().count() + GET_MESSAGE_CHAR_CAP
        );
    }

    #[test]
    fn limit_deserializer_accepts_string_numbers() {
        let args: SearchSessionsArgs =
            serde_json::from_value(serde_json::json!({"query": "q", "limit": "12"})).unwrap();
        assert_eq!(args.limit, Some(12));
        assert!(args.invocation_nonce.is_none());
        assert_eq!(clamp_limit(Some(80), 10, 50), 50);
        assert_eq!(clamp_limit(None, 10, 50), 10);
        assert_eq!(clamp_limit(Some(80), EVENT_LIMIT_DEFAULT, EVENT_LIMIT_MAX), 50);
        assert_eq!(clamp_limit(None, EVENT_LIMIT_DEFAULT, EVENT_LIMIT_MAX), 20);
        let get_args: GetSessionArgs =
            serde_json::from_value(serde_json::json!({"session_id": "s1"})).unwrap();
        assert!(!get_args.include_events);
    }

    #[test]
    fn tools_advertise_read_only_closed_world() {
        for notes in [
            RecallMcp::search_sessions_tool_attr().annotations.unwrap(),
            RecallMcp::get_session_tool_attr().annotations.unwrap(),
            RecallMcp::list_recent_sessions_tool_attr().annotations.unwrap(),
            RecallMcp::file_history_tool_attr().annotations.unwrap(),
        ] {
            assert_eq!(notes.read_only_hint, Some(true));
            assert_eq!(notes.idempotent_hint, Some(true));
            assert_eq!(notes.open_world_hint, Some(false));
        }
    }

    #[test]
    fn capabilities_report_uses_registered_tool_schemas() {
        let report = mcp_capabilities();
        let value = serde_json::to_value(&report).unwrap();
        let tools = value["tools"].as_array().unwrap();
        let get_session = tools
            .iter()
            .find(|tool| tool["name"] == "get_session")
            .expect("get_session capability");
        let search_sessions = tools
            .iter()
            .find(|tool| tool["name"] == "search_sessions")
            .expect("search_sessions capability");
        let list_recent_sessions = tools
            .iter()
            .find(|tool| tool["name"] == "list_recent_sessions")
            .expect("list_recent_sessions capability");

        assert!(get_session["inputSchema"]["properties"]["tail"].is_object());
        assert!(get_session["inputSchema"]["properties"]["include_events"].is_object());
        assert!(search_sessions["inputSchema"]["properties"]["invocation_nonce"].is_object());
        assert!(list_recent_sessions["inputSchema"]["properties"]["invocation_nonce"].is_object());
        assert!(get_session["description"].as_str().unwrap().contains("source_session_id"));
        assert!(get_session["description"].as_str().unwrap().contains("first_message_seq"));
        let description = get_session["description"].as_str().unwrap();
        assert!(description.contains("50 events"));
        assert!(description.contains("200 characters"));
        assert!(description.contains("10000 characters"));
        assert!(description.contains("never returns raw arguments"));
        assert!(report.server.capabilities.tools.is_some());
        let text = render_capabilities(&report);
        assert!(text.contains("get_session"));
        assert!(text.contains("Inputs: "));
        assert!(text.contains("tail"));
        assert!(text.contains("include_events"));
        assert!(text.contains("invocation_nonce"));
        assert!(text.contains("current_session.resolution"));
    }

    fn event(
        seq: u32,
        kind: &str,
        target: Option<&str>,
        timestamp: Option<i64>,
        summary: Option<&str>,
        name: Option<&str>,
    ) -> RawSessionEvent {
        RawSessionEvent {
            event_seq: seq,
            timestamp,
            kind: kind.to_string(),
            actor: "assistant".to_string(),
            name: name.map(str::to_string),
            status: None,
            target: target.map(str::to_string),
            message_seq: Some(1),
            summary: summary.map(str::to_string),
            source_path: None,
            source_event_id: None,
            tool_call_id: None,
            is_meta: None,
            visibility: None,
            attrs_json: Some(r#"{"token":"secret-value"}"#.to_string()),
            parser_version: 1,
        }
    }

    fn persist_events(store: &Store, source: &str, session_id: &str, events: &[RawSessionEvent]) {
        assert!(
            store
                .persist_session_events_for_existing_session(
                    source,
                    &format!("src-{session_id}"),
                    events,
                    1,
                    None,
                )
                .unwrap()
        );
    }

    fn seed_agent_events() -> Store {
        let store = setup();
        store.insert_session(&session("codex-demo", "codex", "codex demo", 2_000)).unwrap();
        store.insert_session(&session("claude-demo", "claude-code", "claude demo", 3_000)).unwrap();
        let mut other = session("codex-other", "codex", "other project", 1_500);
        other.directory = Some("/tmp/other".to_string());
        store.insert_session(&other).unwrap();
        persist_events(
            &store,
            "codex",
            "codex-demo",
            &[
                event(
                    0,
                    "file_write",
                    Some("/tmp/demo/src/db/schema.rs"),
                    Some(5_000),
                    Some("wrote schema"),
                    Some("Edit"),
                ),
                event(
                    1,
                    "command",
                    Some("/tmp/demo/src/db/schema.rs"),
                    Some(6_000),
                    Some("ran cargo test"),
                    Some("Bash"),
                ),
                event(
                    2,
                    "file_write",
                    Some("old_schema.rs"),
                    Some(4_000),
                    Some("renamed"),
                    Some("Edit"),
                ),
                event(
                    3,
                    "file_read",
                    Some(r"src\db\schema.rs"),
                    Some(7_000),
                    Some("win path"),
                    Some("Read"),
                ),
            ],
        );
        persist_events(
            &store,
            "claude-code",
            "claude-demo",
            &[event(
                0,
                "file_read",
                Some("src/db/schema.rs"),
                Some(8_000),
                Some("read schema"),
                Some("Read"),
            )],
        );
        persist_events(
            &store,
            "codex",
            "codex-other",
            &[event(
                0,
                "file_write",
                Some("/tmp/other/src/db/schema.rs"),
                Some(9_000),
                Some("other schema"),
                Some("Edit"),
            )],
        );
        store
    }

    fn file_history_args(path: &str) -> FileHistoryArgs {
        FileHistoryArgs {
            path: path.to_string(),
            project: None,
            source: None,
            kind: None,
            limit: None,
        }
    }

    #[test]
    fn missing_index_is_a_tool_error_for_file_history() {
        let index = IndexState::Unavailable { path: None, message: MISSING_INDEX.to_string() };
        let history = file_history(&index, &file_history_args("src/db/schema.rs"))
            .expect_err("missing index");
        assert!(history.events.is_empty());
        assert_eq!(history.message.as_deref(), Some(MISSING_INDEX));
        assert_eq!(
            json_result(file_history(&index, &file_history_args("src/db/schema.rs"))).is_error,
            Some(true)
        );
    }

    #[test]
    fn file_history_defaults_to_file_kinds_and_suffix_match() {
        let index = ready(seed_agent_events());
        let list = file_history(&index, &file_history_args("src/db/schema.rs")).unwrap();
        assert!(list.message.is_none());
        let targets: Vec<_> = list.events.iter().map(|event| event.target.clone()).collect();
        assert!(targets.contains(&Some("/tmp/demo/src/db/schema.rs".into())));
        assert!(targets.contains(&Some("src/db/schema.rs".into())));
        assert!(targets.contains(&Some("/tmp/other/src/db/schema.rs".into())));
        assert!(!targets.contains(&Some("old_schema.rs".into())));
        assert!(list.events.iter().all(|event| event.kind != "command"));
        assert_eq!(list.events[0].timestamp.as_deref(), Some(iso8601(9_000).as_str()));

        let payload = to_json(&list);
        assert!(
            payload.to_string().contains("wrote schema")
                || payload.to_string().contains("read schema")
        );
        assert!(!payload.to_string().contains("secret-value"));
        assert!(
            payload
                .get("events")
                .and_then(|events| events.get(0))
                .and_then(|event| event.get("attrs_json"))
                .is_none()
        );

        let bare = file_history(&index, &file_history_args("schema.rs")).unwrap();
        assert!(
            bare.events.iter().any(|event| event.target.as_deref() == Some(r"src\db\schema.rs"))
        );
        assert!(bare.events.iter().all(|event| {
            event.target.as_deref().is_some_and(|target| {
                target == "schema.rs"
                    || target.ends_with("/schema.rs")
                    || target.ends_with(r"\schema.rs")
            })
        }));
        assert!(bare.events.iter().all(|event| event.target.as_deref() != Some("old_schema.rs")));

        let absolute =
            file_history(&index, &file_history_args("/abs/elsewhere/src/db/schema.rs")).unwrap();
        assert!(
            absolute.events.iter().any(|event| event.target.as_deref() == Some("src/db/schema.rs"))
        );

        let commands = file_history(
            &index,
            &FileHistoryArgs {
                path: "src/db/schema.rs".into(),
                project: None,
                source: None,
                kind: Some("command".into()),
                limit: None,
            },
        )
        .unwrap();
        assert_eq!(commands.events.len(), 1);
        assert_eq!(commands.events[0].kind, "command");
        assert_eq!(commands.events[0].session_id, "codex-demo");
    }

    #[test]
    fn file_history_honors_project_source_and_unknown_source() {
        let index = ready(seed_agent_events());
        let scoped = file_history(
            &index,
            &FileHistoryArgs {
                path: "src/db/schema.rs".into(),
                project: Some("/tmp/demo".into()),
                source: None,
                kind: None,
                limit: None,
            },
        )
        .unwrap();
        assert_eq!(scoped.events.len(), 2);
        assert!(scoped.events.iter().all(|event| event.project.as_deref() == Some("/tmp/demo")));

        let by_source = file_history(
            &index,
            &FileHistoryArgs {
                path: "src/db/schema.rs".into(),
                project: Some("/tmp/demo".into()),
                source: Some("claude-code".into()),
                kind: None,
                limit: None,
            },
        )
        .unwrap();
        assert_eq!(by_source.events.len(), 1);
        assert_eq!(by_source.events[0].source, "claude-code");

        let unknown = file_history(
            &index,
            &FileHistoryArgs {
                path: "src/db/schema.rs".into(),
                project: None,
                source: Some("not-a-source".into()),
                kind: None,
                limit: None,
            },
        )
        .expect_err("unknown source");
        assert!(unknown.events.is_empty());
        assert!(unknown.message.unwrap().contains("unknown source"));
        assert_eq!(
            json_result(file_history(&index, &file_history_args("src/db/schema.rs"))).is_error,
            Some(false)
        );
    }

    #[test]
    fn file_history_clamps_limit_and_rejects_blank_path() {
        let store = setup();
        store.insert_session(&session("s1", "codex", "many", 1_000)).unwrap();
        let events: Vec<_> = (0..51)
            .map(|seq| {
                event(
                    seq,
                    "file_read",
                    Some("src/db/schema.rs"),
                    Some(10_000 + i64::from(seq)),
                    Some("read"),
                    Some("Read"),
                )
            })
            .collect();
        persist_events(&store, "codex", "s1", &events);
        let index = ready(store);
        let list = file_history(
            &index,
            &FileHistoryArgs {
                path: "src/db/schema.rs".into(),
                project: None,
                source: None,
                kind: None,
                limit: Some(80),
            },
        )
        .unwrap();
        assert_eq!(list.events.len(), 50);

        let blank = file_history(&index, &file_history_args("   ")).expect_err("blank path");
        assert_eq!(blank.message.as_deref(), Some(PATH_REQUIRED));
        assert_eq!(
            json_result(file_history(&index, &file_history_args("   "))).is_error,
            Some(true)
        );
    }

    #[test]
    fn file_history_truncates_long_summaries() {
        let store = setup();
        store.insert_session(&session("s1", "codex", "long", 1_000)).unwrap();
        persist_events(
            &store,
            "codex",
            "s1",
            &[event(
                0,
                "tool_call",
                Some("src/db/schema.rs"),
                Some(1_000),
                Some(&"z".repeat(EXCERPT_CHAR_CAP + 20)),
                Some("Tool"),
            )],
        );
        let list = file_history(
            &ready(store),
            &FileHistoryArgs {
                path: "src/db/schema.rs".into(),
                project: None,
                source: None,
                kind: Some("tool_call".into()),
                limit: None,
            },
        )
        .unwrap();
        let summary = list.events[0].summary.as_deref().unwrap();
        assert_eq!(summary.chars().count(), EXCERPT_CHAR_CAP);
        assert!(summary.ends_with('…'));
    }
}
