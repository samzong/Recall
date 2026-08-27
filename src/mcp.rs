use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use chrono::SecondsFormat;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{
    ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router, transport::stdio,
};
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::adapters;
use crate::db::search::{SearchEngine, SearchFilters, TimeRange};
use crate::db::store::Store;
use crate::project_scope::ProjectScope;
use crate::query::{query_embedding, resolve_source_filter};
use crate::types::{Message, Role, Session};

const SEARCH_LIMIT_DEFAULT: u32 = 10;
const SEARCH_LIMIT_MAX: u32 = 50;
const LIST_LIMIT_DEFAULT: u32 = 10;
const LIST_LIMIT_MAX: u32 = 50;
const GET_MAX_MESSAGES_DEFAULT: u32 = 50;
const GET_MESSAGE_CHAR_CAP: usize = 2_000;
const GET_RESPONSE_CHAR_CAP: usize = 32_000;
const EXCERPT_CHAR_CAP: usize = 200;

const MISSING_INDEX: &str =
    "Recall index not found. Run `recall sync` in a terminal to create it, then retry.";
const EMPTY_INDEX: &str = "No sessions in the Recall index. Run `recall sync` in a terminal after using a supported coding agent.";
const SEARCH_EMPTY: &str = "No matching sessions.";
const SESSION_NOT_FOUND: &str = "Session not found.";

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
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetSessionArgs {
    /// Recall session id from search_sessions or list_recent_sessions.
    session_id: String,
    /// Maximum messages to return from the start of the session. Defaults to 50.
    #[serde(default, deserialize_with = "deserialize_opt_u32")]
    max_messages: Option<u32>,
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
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
struct SessionHit {
    session_id: String,
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
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
struct SessionDetail {
    message: Option<String>,
    session_id: Option<String>,
    source: Option<String>,
    project: Option<String>,
    title: Option<String>,
    summary: Option<String>,
    timestamp: Option<String>,
    message_count: Option<u32>,
    returned_messages: usize,
    truncated: bool,
    messages: String,
}

enum IndexState {
    Ready(Store),
    Unavailable { path: Option<PathBuf>, message: String },
}

#[derive(Clone)]
struct RecallMcp {
    index: Arc<Mutex<IndexState>>,
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

async fn serve(db: Option<PathBuf>) -> Result<()> {
    let service = RecallMcp::new(db.as_deref()).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[tool_router]
impl RecallMcp {
    fn new(db: Option<&Path>) -> Self {
        Self { index: Arc::new(Mutex::new(IndexState::open(db))), tool_router: Self::tool_router() }
    }

    #[tool(
        description = "Search past AI coding sessions across Claude Code, Codex, OpenCode, Cursor, and other indexed tools. Use when the user asks whether they have seen an error, made a decision, or solved a similar problem before. Returns session id, source, project, title, matched excerpt, and ISO-8601 timestamp.",
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
        Ok(json_result(search_sessions(&lock_index(&self.index), &args)))
    }

    #[tool(
        description = "Load one indexed session by Recall session_id. Use after search_sessions or list_recent_sessions when you need the conversation itself. Each message is truncated at 2000 characters; the combined message text is capped at 32000 characters. Returns metadata plus messages as plain text in sequence order.",
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
        description = "List the most recently active indexed sessions, newest first. Use to browse what the user has been working on when there is no search query. Each hit has the same shape as search_sessions: session id, source, project, title, excerpt (summary when present), and ISO-8601 timestamp.",
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
        Ok(json_result(list_recent_sessions(&lock_index(&self.index), &args)))
    }
}

#[tool_handler]
impl ServerHandler for RecallMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("recall", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Read-only memory over the local Recall session index. Search or list past coding sessions, then fetch a session when you need the transcript. This server never writes, syncs, or mutates the index.",
            )
    }
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
    HitList { message: Some(message.into()), hits: Vec::new() }
}

fn search_sessions(
    index: &IndexState,
    args: &SearchSessionsArgs,
) -> std::result::Result<HitList, HitList> {
    match index {
        IndexState::Unavailable { message, .. } => Err(empty_hits(message.clone())),
        IndexState::Ready(store) => search_ready(store, args).map_err(empty_hits),
    }
}

fn search_ready(store: &Store, args: &SearchSessionsArgs) -> std::result::Result<HitList, String> {
    let (scope, sources) = resolve_filters(store, args.project.as_deref(), args.source.as_deref())?;
    let limit = clamp_limit(args.limit, SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX);
    let embedding = query_embedding(store, &args.query, |message| {
        tracing::info!("{message}");
    })
    .map_err(|error| error.to_string())?;
    let filters = SearchFilters { sources, time_range: TimeRange::All, scope, thread_role: None };
    let results = SearchEngine::new(&store.conn)
        .hybrid_search(&args.query, embedding.as_deref(), &filters, limit, 3)
        .map_err(|error| error.to_string())?;
    if results.is_empty() {
        let message = if store_is_empty(store) { EMPTY_INDEX } else { SEARCH_EMPTY };
        return Ok(HitList { message: Some(message.to_string()), hits: Vec::new() });
    }
    Ok(HitList {
        message: None,
        hits: results
            .into_iter()
            .map(|result| session_hit(&result.session, result.snippet))
            .collect(),
    })
}

fn list_recent_sessions(
    index: &IndexState,
    args: &ListRecentSessionsArgs,
) -> std::result::Result<HitList, HitList> {
    match index {
        IndexState::Unavailable { message, .. } => Err(empty_hits(message.clone())),
        IndexState::Ready(store) => list_ready(store, args).map_err(empty_hits),
    }
}

fn list_ready(
    store: &Store,
    args: &ListRecentSessionsArgs,
) -> std::result::Result<HitList, String> {
    let (scope, sources) = resolve_filters(store, args.project.as_deref(), args.source.as_deref())?;
    let limit = clamp_limit(args.limit, LIST_LIMIT_DEFAULT, LIST_LIMIT_MAX);
    let sessions = store
        .list_recent_sessions_for_search_scope(sources.as_deref(), TimeRange::All, &scope, limit)
        .map_err(|error| error.to_string())?;
    if sessions.is_empty() {
        let message = if store_is_empty(store) { EMPTY_INDEX } else { SEARCH_EMPTY };
        return Ok(HitList { message: Some(message.to_string()), hits: Vec::new() });
    }
    Ok(HitList {
        message: None,
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
    let (text, returned, truncated) = render_messages(&messages, max_messages);
    Ok(SessionDetail {
        message: None,
        session_id: Some(session.id.clone()),
        source: Some(session.source.clone()),
        project: session.directory.clone(),
        title: Some(session_title(&session)),
        summary: session.summary.clone(),
        timestamp: Some(iso8601(session.started_at)),
        message_count: Some(session.message_count),
        returned_messages: returned,
        truncated,
        messages: text,
    })
}

fn empty_detail(message: Option<String>) -> SessionDetail {
    SessionDetail {
        message,
        session_id: None,
        source: None,
        project: None,
        title: None,
        summary: None,
        timestamp: None,
        message_count: None,
        returned_messages: 0,
        truncated: false,
        messages: String::new(),
    }
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

fn render_messages(messages: &[Message], max_messages: usize) -> (String, usize, bool) {
    let mut text = String::new();
    let mut returned = 0;
    let mut truncated = messages.len() > max_messages;
    for message in messages.iter().take(max_messages) {
        let (body, cut) = truncate_chars(&message.content, GET_MESSAGE_CHAR_CAP);
        if cut {
            truncated = true;
        }
        let block = format!("[{}] {body}", role_label(&message.role));
        let extra = if text.is_empty() { block.len() } else { block.len() + 2 };
        if !text.is_empty() && text.len() + extra > GET_RESPONSE_CHAR_CAP {
            truncated = true;
            break;
        }
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        if text.len() + block.len() > GET_RESPONSE_CHAR_CAP {
            let remaining = GET_RESPONSE_CHAR_CAP.saturating_sub(text.len());
            text.push_str(&truncate_chars(&block, remaining).0);
            truncated = true;
            returned += 1;
            break;
        }
        text.push_str(&block);
        returned += 1;
    }
    (text, returned, truncated)
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
    use crate::types::Session;

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

    #[test]
    fn missing_index_is_a_tool_error() {
        let index = IndexState::Unavailable { path: None, message: MISSING_INDEX.to_string() };
        let args =
            SearchSessionsArgs { query: "error".into(), project: None, source: None, limit: None };
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
        };
        let search = search_sessions(&index, &search_args).unwrap();
        assert!(search.hits.is_empty());
        assert_eq!(search.message.as_deref(), Some(EMPTY_INDEX));
        assert_eq!(json_result(search_sessions(&index, &search_args)).is_error, Some(false));

        let list_args = ListRecentSessionsArgs { project: None, source: None, limit: None };
        let listed = list_recent_sessions(&index, &list_args).unwrap();
        assert!(listed.hits.is_empty());
        assert_eq!(listed.message.as_deref(), Some(EMPTY_INDEX));
        assert_eq!(json_result(list_recent_sessions(&index, &list_args)).is_error, Some(false));
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
            },
        )
        .unwrap();
        assert_eq!(list.hits.len(), 1);
        assert_eq!(list.hits[0].session_id, "s1");
        assert_eq!(list.hits[0].source, "codex");
        assert_eq!(list.hits[0].project.as_deref(), Some("/tmp/demo"));
        assert_eq!(list.hits[0].title, "iterator panic");
        assert!(list.hits[0].excerpt.as_deref().unwrap().contains("iterators"));
        assert_eq!(list.hits[0].timestamp, iso8601(2_000));
        assert!(list.message.is_none());
    }

    #[test]
    fn list_recent_is_newest_first_and_matches_search_shape() {
        let store = setup();
        store.insert_session(&session("old", "codex", "older", 1_000)).unwrap();
        store.insert_session(&session("new", "claude-code", "newer", 9_000)).unwrap();
        let index = ready(store);

        let list = list_recent_sessions(
            &index,
            &ListRecentSessionsArgs { project: None, source: None, limit: Some(10) },
        )
        .unwrap();
        assert_eq!(list.hits.len(), 2);
        assert_eq!(list.hits[0].session_id, "new");
        assert_eq!(list.hits[0].excerpt.as_deref(), Some("newer summary"));
        assert_eq!(list.hits[1].session_id, "old");
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

        let detail =
            get_session(&index, &GetSessionArgs { session_id: "s1".into(), max_messages: Some(1) })
                .unwrap();
        assert_eq!(detail.session_id.as_deref(), Some("s1"));
        assert_eq!(detail.returned_messages, 1);
        assert!(detail.truncated);
        assert!(detail.messages.starts_with("[user] "));
        assert!(detail.messages.ends_with('…'));
        assert!(!detail.messages.contains("[assistant]"));
        assert!(
            detail.messages.chars().count() <= "[user] ".chars().count() + GET_MESSAGE_CHAR_CAP
        );
    }

    #[test]
    fn get_session_missing_id_is_a_tool_error() {
        let store = setup();
        store.insert_session(&session("s1", "codex", "title", 1_000)).unwrap();
        let index = ready(store);
        let args = GetSessionArgs { session_id: "missing".into(), max_messages: None };
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
            &ListRecentSessionsArgs { project: None, source: None, limit: None },
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
            &ListRecentSessionsArgs { project: None, source: None, limit: None },
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
        let detail =
            get_session(&index, &GetSessionArgs { session_id: "s1".into(), max_messages: None })
                .unwrap();
        assert!(detail.truncated);
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
        assert_eq!(clamp_limit(Some(80), 10, 50), 50);
        assert_eq!(clamp_limit(None, 10, 50), 10);
    }

    #[test]
    fn tools_advertise_read_only_closed_world() {
        let notes = RecallMcp::search_sessions_tool_attr().annotations.unwrap();
        assert_eq!(notes.read_only_hint, Some(true));
        assert_eq!(notes.idempotent_hint, Some(true));
        assert_eq!(notes.open_world_hint, Some(false));
    }
}
