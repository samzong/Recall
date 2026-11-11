use super::*;
use crate::skill_audit::SkillAuditReport;
use crate::tui::viewing_state::{ViewingSessionSummary, build_viewing_caches};
use crate::types::Role;

fn source(id: &str, label: &str) -> (String, String) {
    (id.to_string(), label.to_string())
}

fn app_with_sources() -> App {
    App::empty(
        vec![source("claude", "Claude"), source("cursor", "Cursor"), source("codex", "Codex")],
        AppConfig::default(),
        ProjectScope::Global,
    )
}

fn app_and_store() -> (App, Store) {
    crate::db::schema::register_sqlite_vec();
    (app_with_sources(), Store::open_in_memory().unwrap())
}

fn codex_search_result() -> SearchResult {
    SearchResult {
        session: crate::types::Session {
            id: "session1".to_string(),
            source: "codex".to_string(),
            source_id: "019e6d8d-588b-7fd2-a326-c525469ed120".to_string(),
            title: "Codex thread".to_string(),
            directory: Some("/tmp/project".to_string()),
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
            started_at: 0,
            updated_at: None,
            message_count: 1,
            entrypoint: None,
            custom_title: None,
            summary: None,
            duration_minutes: None,
            source_file_path: None,
            is_import: false,
            locations: Vec::new(),
            alternative_versions: 0,
        },
        match_source: MatchSource::Fts,
        snippet: None,
    }
}

fn search_result_with_times(
    source_id: &str,
    started_at: i64,
    updated_at: Option<i64>,
) -> SearchResult {
    let mut result = codex_search_result();
    result.session.source_id = source_id.to_string();
    result.session.started_at = started_at;
    result.session.updated_at = updated_at;
    result
}

fn message(role: Role, timestamp: Option<i64>, seq: u32) -> Message {
    Message {
        session_id: "session1".to_string(),
        role,
        content: "hello".to_string(),
        timestamp,
        seq,
    }
}

fn tall_message(role: Role, seq: u32) -> Message {
    message_with_lines(role, seq, 6)
}

fn message_with_lines(role: Role, seq: u32, line_count: u32) -> Message {
    let mut msg = message(role, None, seq);
    msg.content = (0..line_count).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
    msg
}

fn numbered_results(count: usize) -> Vec<SearchResult> {
    (0..count)
        .map(|n| {
            let mut result = codex_search_result();
            result.session.id = format!("session{n}");
            result.session.title = format!("Session {n}");
            result
        })
        .collect()
}

fn copilot_search_result() -> SearchResult {
    let mut result = codex_search_result();
    result.session.source = "copilot-cli".to_string();
    result.session.source_id = "7d5c993a-0966-4e3e-9622-7a39ba9576ba".to_string();
    result.session.title = "Copilot session".to_string();
    result
}

mod actions;
mod filters_search;
mod navigation;
