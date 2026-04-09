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
