#[derive(Debug, Clone, PartialEq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn as_str(&self) -> &str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TokenSource {
    Observed,
    Derived,
    Estimated,
}

impl TokenSource {
    pub fn as_str(self) -> &'static str {
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
pub struct Session {
    pub id: String,
    pub source: String,
    pub source_id: String,
    pub title: String,
    pub directory: Option<String>,
    pub started_at: i64,
    pub updated_at: Option<i64>,
    pub message_count: u32,
    pub entrypoint: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub session_id: String,
    pub role: Role,
    pub content: String,
    pub timestamp: Option<i64>,
    pub seq: u32,
}

#[derive(Debug, Clone)]
pub struct RawUsageEvent {
    pub event_key: String,
    pub event_seq: u32,
    pub message_seq: Option<u32>,
    pub timestamp: i64,
    pub model: String,
    pub provider: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub reasoning_tokens: i64,
    pub token_source: TokenSource,
    pub parser_version: u32,
    pub source_path: Option<String>,
    pub raw_usage_json: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UsageEventRecord {
    pub session_id: String,
    pub source: String,
    pub source_id: String,
    pub event_key: String,
    pub timestamp: i64,
    pub model: String,
    pub provider: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub reasoning_tokens: i64,
    pub token_source: String,
}

#[derive(Debug)]
pub enum MatchSource {
    Fts,
    Vector,
    Hybrid,
}

#[derive(Debug)]
pub struct SearchResult {
    pub session: Session,
    pub match_source: MatchSource,
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SemanticProgress {
    pub total_sessions: u64,
    pub done_sessions: u64,
    pub processing_sessions: u64,
    pub failed_sessions: u64,
    pub pending_sessions: u64,
    pub current_session_title: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SemanticSessionJob {
    pub session_id: String,
    pub title: String,
    pub units_total: u64,
}

#[derive(Debug, Clone, Default)]
pub struct BackgroundJobStatus {
    pub phase: Option<String>,
    pub detail: Option<String>,
}
