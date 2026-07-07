#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Role {
    User,
    Assistant,
}

impl Role {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum TokenSource {
    Observed,
    Derived,
    Estimated,
}

impl TokenSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TokenSource::Observed => "observed",
            TokenSource::Derived => "derived",
            TokenSource::Estimated => "estimated",
        }
    }
}

impl std::str::FromStr for TokenSource {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "observed" => Ok(TokenSource::Observed),
            "derived" => Ok(TokenSource::Derived),
            "estimated" => Ok(TokenSource::Estimated),
            _ => Err(()),
        }
    }
}

impl std::str::FromStr for Role {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "user" => Ok(Role::User),
            "assistant" => Ok(Role::Assistant),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Session {
    pub(crate) id: String,
    pub(crate) source: String,
    pub(crate) source_id: String,
    pub(crate) title: String,
    pub(crate) directory: Option<String>,
    pub(crate) repo_remote: Option<String>,
    pub(crate) repo_slug: Option<String>,
    pub(crate) repo_name: Option<String>,
    pub(crate) started_at: i64,
    pub(crate) updated_at: Option<i64>,
    pub(crate) message_count: u32,
    pub(crate) entrypoint: Option<String>,
    pub(crate) custom_title: Option<String>,
    pub(crate) summary: Option<String>,
    pub(crate) duration_minutes: Option<u32>,
    pub(crate) source_file_path: Option<String>,
    pub(crate) is_import: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Message {
    pub(crate) session_id: String,
    pub(crate) role: Role,
    pub(crate) content: String,
    pub(crate) timestamp: Option<i64>,
    pub(crate) seq: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct RawUsageEvent {
    pub(crate) event_key: String,
    pub(crate) event_seq: u32,
    pub(crate) message_seq: Option<u32>,
    pub(crate) timestamp: i64,
    pub(crate) model: String,
    pub(crate) provider: String,
    pub(crate) input_tokens: i64,
    pub(crate) output_tokens: i64,
    pub(crate) cache_read_tokens: i64,
    pub(crate) cache_write_tokens: i64,
    pub(crate) reasoning_tokens: i64,
    pub(crate) token_source: TokenSource,
    pub(crate) parser_version: u32,
    pub(crate) source_path: Option<String>,
    pub(crate) raw_usage_json: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct RawSessionEvent {
    pub(crate) event_seq: u32,
    pub(crate) timestamp: Option<i64>,
    pub(crate) kind: String,
    pub(crate) actor: String,
    pub(crate) name: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) target: Option<String>,
    pub(crate) message_seq: Option<u32>,
    pub(crate) summary: Option<String>,
    pub(crate) source_path: Option<String>,
    pub(crate) source_event_id: Option<String>,
    pub(crate) attrs_json: Option<String>,
    pub(crate) parser_version: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct UsageEventRecord {
    pub(crate) session_id: String,
    pub(crate) source: String,
    #[allow(dead_code)] // persisted for future per-source usage breakdown
    pub(crate) source_id: String,
    pub(crate) event_key: String,
    pub(crate) timestamp: i64,
    pub(crate) model: String,
    pub(crate) provider: String,
    pub(crate) input_tokens: i64,
    pub(crate) output_tokens: i64,
    pub(crate) cache_read_tokens: i64,
    pub(crate) cache_write_tokens: i64,
    pub(crate) reasoning_tokens: i64,
    pub(crate) token_source: String,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionEventRecord {
    pub(crate) event_seq: u32,
    pub(crate) timestamp: Option<i64>,
    pub(crate) kind: String,
    pub(crate) actor: String,
    pub(crate) name: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) target: Option<String>,
    pub(crate) message_seq: Option<u32>,
    pub(crate) summary: Option<String>,
    pub(crate) source_path: Option<String>,
    pub(crate) source_event_id: Option<String>,
    pub(crate) attrs_json: Option<String>,
    pub(crate) parser_version: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionUsageEventRecord {
    pub(crate) event_key: String,
    pub(crate) event_seq: u32,
    pub(crate) message_seq: Option<u32>,
    pub(crate) timestamp: i64,
    pub(crate) model: String,
    pub(crate) provider: String,
    pub(crate) input_tokens: i64,
    pub(crate) output_tokens: i64,
    pub(crate) cache_read_tokens: i64,
    pub(crate) cache_write_tokens: i64,
    pub(crate) reasoning_tokens: i64,
    pub(crate) token_source: String,
    pub(crate) parser_version: u32,
    pub(crate) source_path: Option<String>,
    pub(crate) raw_usage_json: Option<String>,
}

#[derive(Debug)]
pub(crate) enum MatchSource {
    Fts,
    Vector,
    Hybrid,
}

#[derive(Debug)]
pub(crate) struct SearchResult {
    pub(crate) session: Session,
    pub(crate) match_source: MatchSource,
    pub(crate) snippet: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SemanticProgress {
    pub(crate) total_sessions: u64,
    pub(crate) done_sessions: u64,
    pub(crate) processing_sessions: u64,
    pub(crate) failed_sessions: u64,
    pub(crate) pending_sessions: u64,
    #[allow(dead_code)] // populated for future status-bar detail
    pub(crate) current_session_title: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SemanticSessionJob {
    pub(crate) session_id: String,
    pub(crate) title: String,
    pub(crate) units_total: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct BackgroundJobStatus {
    pub(crate) phase: Option<String>,
    #[allow(dead_code)] // loaded from background_jobs.detail
    pub(crate) detail: Option<String>,
}
