use anyhow::{Result, bail};
use serde::Deserialize;

pub const RECORD_SCHEMA_VERSION: u32 = 5;
pub const MAX_SOURCE_ID_LENGTH: usize = 256;
pub const MAX_CONTENT_LENGTH: usize = 200_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RoleSet {
    user: bool,
    assistant: bool,
}

impl RoleSet {
    pub fn parse(value: &str) -> Result<Self> {
        let mut roles = Self { user: false, assistant: false };
        for part in value.split(',') {
            match part.trim().to_ascii_lowercase().as_str() {
                "" => {}
                "user" => roles.user = true,
                "assistant" => roles.assistant = true,
                other => bail!("unsupported role '{other}'; expected user and/or assistant"),
            }
        }
        if !roles.user && !roles.assistant {
            bail!("--roles must include user and/or assistant");
        }
        Ok(roles)
    }

    pub fn contains(self, role: &str) -> bool {
        match role {
            "user" => self.user,
            "assistant" => self.assistant,
            _ => false,
        }
    }

    pub fn as_report_value(self) -> String {
        match (self.user, self.assistant) {
            (true, true) => "user,assistant",
            (true, false) => "user",
            (false, true) => "assistant",
            (false, false) => unreachable!(),
        }
        .to_string()
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ExportRecord {
    pub schema_version: u32,
    #[serde(default)]
    pub record_type: String,
    pub session: ExportSession,
    #[serde(default)]
    pub messages: Vec<ExportMessage>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ExportSession {
    pub id: String,
    pub source: String,
    #[serde(default)]
    pub started_at: i64,
    #[serde(default)]
    pub topology: Option<ExportTopology>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ExportTopology {
    #[serde(default)]
    pub thread_role: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ExportMessage {
    pub seq: u32,
    pub role: String,
    pub content: String,
}

#[derive(Clone, Debug)]
pub struct CaptureRequest {
    pub adapter: String,
    pub session_id: String,
    pub seq: u32,
    pub source_id: String,
    pub content: String,
    pub started_at: Option<String>,
}

#[derive(Clone, Debug)]
pub enum CaptureItem {
    Request(CaptureRequest),
    Failed(String),
}

pub fn parse_export_line(line: &str, line_no: usize) -> Result<ExportRecord> {
    let record: ExportRecord = serde_json::from_str(line)
        .map_err(|error| anyhow::anyhow!("failed to parse export JSONL line {line_no}: {error}"))?;
    if record.schema_version != RECORD_SCHEMA_VERSION {
        bail!(
            "unsupported export schema_version {} on line {line_no}; expected {RECORD_SCHEMA_VERSION}",
            record.schema_version
        );
    }
    if record.record_type != "session" {
        bail!(
            "unsupported export record_type '{}' on line {line_no}; expected session",
            record.record_type
        );
    }
    Ok(record)
}

pub fn session_captures(record: &ExportRecord, roles: RoleSet) -> Result<Vec<CaptureItem>> {
    if record.session.topology.as_ref().and_then(|topology| topology.thread_role.as_deref())
        == Some("subagent")
    {
        return Ok(Vec::new());
    }
    let adapter = record.session.source.trim();
    let session_id = record.session.id.trim();
    if adapter.is_empty() || session_id.is_empty() {
        bail!("export session is missing source or id");
    }
    let started_at = if record.session.started_at > 0 {
        Some(record.session.started_at.to_string())
    } else {
        None
    };
    let mut messages: Vec<&ExportMessage> = record
        .messages
        .iter()
        .filter(|message| roles.contains(&message.role) && !message.content.trim().is_empty())
        .collect();
    messages.sort_by_key(|message| message.seq);
    let mut items = Vec::new();
    for message in messages {
        items.push(capture_item(adapter, session_id, started_at.clone(), message)?);
    }
    Ok(items)
}

fn capture_item(
    adapter: &str,
    session_id: &str,
    started_at: Option<String>,
    message: &ExportMessage,
) -> Result<CaptureItem> {
    let content = message.content.trim();
    if content.chars().count() > MAX_CONTENT_LENGTH {
        return Ok(CaptureItem::Failed(format!(
            "message {session_id}#{} exceeds {MAX_CONTENT_LENGTH} characters",
            message.seq
        )));
    }
    let source_id = format!("recall:{adapter}:{session_id}:{}", message.seq);
    if source_id.chars().count() > MAX_SOURCE_ID_LENGTH {
        bail!("source_id exceeds {MAX_SOURCE_ID_LENGTH} characters: {source_id}");
    }
    Ok(CaptureItem::Request(CaptureRequest {
        adapter: adapter.to_string(),
        session_id: session_id.to_string(),
        seq: message.seq,
        source_id,
        content: content.to_string(),
        started_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::RoleSet;
    use super::{
        CaptureItem, MAX_CONTENT_LENGTH, RECORD_SCHEMA_VERSION, parse_export_line, session_captures,
    };

    fn record(source: &str, id: &str, role: &str, messages: &str) -> String {
        format!(
            r#"{{"schema_version":{RECORD_SCHEMA_VERSION},"record_type":"session","session":{{"id":"{id}","source":"{source}","started_at":1000,"topology":{{"thread_role":"{role}","parents":[]}}}},"messages":{messages}}}"#
        )
    }

    fn requests(record: &str, roles: RoleSet) -> Vec<super::CaptureRequest> {
        session_captures(&parse_export_line(record, 1).unwrap(), roles)
            .unwrap()
            .into_iter()
            .filter_map(|item| match item {
                CaptureItem::Request(request) => Some(request),
                CaptureItem::Failed(_) => None,
            })
            .collect()
    }

    fn user_only() -> RoleSet {
        RoleSet::parse("user").unwrap()
    }

    #[test]
    fn rejects_schema_other_than_five() {
        let line = r#"{"schema_version":4,"record_type":"session","session":{"id":"s","source":"codex"},"messages":[]}"#;
        let error = parse_export_line(line, 1).unwrap_err().to_string();
        assert!(error.contains("schema_version 4"), "{error}");
    }

    #[test]
    fn skips_subagent_and_empty_user_bodies() {
        let jsonl = [
            record("cursor", "s1", "subagent", r#"[{"seq":0,"role":"user","content":"hi"}]"#),
            record("cursor", "s2", "primary", r#"[{"seq":0,"role":"assistant","content":"no"}]"#),
        ]
        .join("\n");
        let records: Vec<_> = jsonl
            .lines()
            .enumerate()
            .map(|(index, line)| parse_export_line(line, index + 1).unwrap())
            .collect();
        assert!(session_captures(&records[0], user_only()).unwrap().is_empty());
        assert!(session_captures(&records[1], user_only()).unwrap().is_empty());
    }

    #[test]
    fn posts_one_source_per_user_message() {
        let line = format!(
            r#"{{"schema_version":{RECORD_SCHEMA_VERSION},"record_type":"session","session":{{"id":"s1","source":"gemini-cli","started_at":9}},"messages":[{{"seq":1,"role":"assistant","content":"no"}},{{"seq":0,"role":"user","content":" one "}},{{"seq":2,"role":"user","content":"two"}}]}}"#
        );
        let captured = requests(&line, user_only());
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].source_id, "recall:gemini-cli:s1:0");
        assert_eq!(captured[0].content, "one");
        assert_eq!(captured[0].seq, 0);
        assert_eq!(captured[1].source_id, "recall:gemini-cli:s1:2");
        assert_eq!(captured[1].content, "two");
        assert_eq!(captured[1].started_at.as_deref(), Some("9"));
    }

    #[test]
    fn assistant_roles_are_opt_in_as_separate_sources() {
        let line = record(
            "codex",
            "s1",
            "primary",
            r#"[{"seq":0,"role":"user","content":"q"},{"seq":1,"role":"assistant","content":"a"}]"#,
        );
        let user = requests(&line, user_only());
        assert_eq!(user.len(), 1);
        assert_eq!(user[0].source_id, "recall:codex:s1:0");
        let both = requests(&line, RoleSet::parse("user,assistant").unwrap());
        assert_eq!(both.len(), 2);
        assert_eq!(both[1].source_id, "recall:codex:s1:1");
        assert_eq!(both[1].content, "a");
    }

    #[test]
    fn oversized_message_fails_alone() {
        let huge = "x".repeat(MAX_CONTENT_LENGTH + 1);
        let line = format!(
            r#"{{"schema_version":{RECORD_SCHEMA_VERSION},"record_type":"session","session":{{"id":"s1","source":"codex","started_at":1,"topology":{{"thread_role":"primary","parents":[]}}}},"messages":[{{"seq":0,"role":"user","content":"{huge}"}},{{"seq":1,"role":"user","content":"ok"}}]}}"#
        );
        let items = session_captures(&parse_export_line(&line, 1).unwrap(), user_only()).unwrap();
        assert!(matches!(items[0], CaptureItem::Failed(_)));
        let CaptureItem::Request(request) = &items[1] else {
            panic!("expected request");
        };
        assert_eq!(request.source_id, "recall:codex:s1:1");
    }

    #[test]
    fn source_id_limit_counts_characters() {
        let session_id = "é".repeat(150);
        let line = record(
            "codex",
            &session_id,
            "primary",
            r#"[{"seq":0,"role":"user","content":"hello"}]"#,
        );
        let captured = requests(&line, user_only());
        assert_eq!(captured.len(), 1);
        assert!(captured[0].source_id.chars().count() <= super::MAX_SOURCE_ID_LENGTH);
        assert!(captured[0].source_id.len() > super::MAX_SOURCE_ID_LENGTH);
    }

    #[test]
    fn rejects_unknown_roles() {
        let error = RoleSet::parse("user,tool").unwrap_err().to_string();
        assert!(error.contains("tool"), "{error}");
    }
}
