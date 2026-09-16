use super::{
    FileHistoryQuery, SearchEngine, SearchFilters, SessionEventQuery, TimeRange, tokenize_query,
    trigram_fts5_query, unicode61_fts5_query,
};
use crate::db::schema;
use crate::db::store::Store;
use crate::project_scope::ProjectScope;
use crate::types::{RawSessionEvent, Session};

#[test]
fn tokenize_query_strips_punctuation_and_case() {
    assert_eq!(
        tokenize_query("context power café Codex "),
        vec!["context", "power", "café", "codex"]
    );
    assert_eq!(tokenize_query("bug OR 1=1 --"), vec!["bug", "or", "11"]);
}

#[test]
fn fts5_queries_split_trigram_and_short_tokens() {
    let tokens = tokenize_query("context power café Codex ");
    assert_eq!(trigram_fts5_query(&tokens), "");
    assert_eq!(
        unicode61_fts5_query(&tokens, true),
        r#""context" OR "power" OR "café" OR "codex"*"#
    );
    let short = tokenize_query("rx \u{77e9}\u{9635}");
    assert_eq!(trigram_fts5_query(&short), "");
    assert_eq!(unicode61_fts5_query(&short, true), format!(r#""rx" OR "{}"*"#, "\u{77e9}\u{9635}"));
    let keywords = tokenize_query("AND OR NOT");
    assert_eq!(trigram_fts5_query(&keywords), "");
    assert_eq!(unicode61_fts5_query(&keywords, true), r#""and" OR "or" OR "not"*"#);
    let phrase_text = "\u{7edf}\u{8ba1}\u{7684}\u{4e0d}\u{51c6}\u{786e}";
    let phrase = tokenize_query(phrase_text);
    assert_eq!(trigram_fts5_query(&phrase), format!("\"{phrase_text}\""));
}

#[test]
fn search_keeps_short_mixed_and_deferred_legacy_terms_without_embeddings() {
    crate::db::schema::register_sqlite_vec();
    let store = Store::open_in_memory().unwrap();
    store
        .conn
        .execute_batch(
            "INSERT INTO sessions (id, source, source_id, title, started_at)
             VALUES
                ('short', 'test', 'short', 'Short', 1),
                ('cjk', 'test', 'cjk', 'CJK', 2),
                ('long', 'test', 'long', 'Long', 3);
             INSERT INTO messages (session_id, role, content, seq)
             VALUES
                ('short', 'user', 'rx routing', 0),
                ('long', 'user', 'context cache enables powercontext', 0);",
        )
        .unwrap();
    let cjk = "\u{77e9}\u{9635}\u{8fd0}\u{7b97}";
    store
        .conn
        .execute(
            "INSERT INTO messages (session_id, role, content, seq, trigram_indexed)
             VALUES ('cjk', 'user', ?1, 0, ?2)",
            rusqlite::params![cjk, crate::utils::text_needs_trigram(cjk)],
        )
        .unwrap();
    let filters = SearchFilters {
        sources: None,
        time_range: TimeRange::All,
        scope: ProjectScope::Global,
        thread_role: None,
        excluded_session_id: None,
    };
    let engine = SearchEngine::new(&store.conn);

    let short = engine.hybrid_search("rx", None, &filters, 10, 3).unwrap();
    assert_eq!(short[0].session.id, "short");
    let cjk = engine.hybrid_search("\u{77e9}\u{9635}", None, &filters, 10, 3).unwrap();
    assert_eq!(cjk[0].session.id, "cjk");
    let mut mixed: Vec<_> = engine
        .hybrid_search("context rx", None, &filters, 10, 3)
        .unwrap()
        .into_iter()
        .map(|result| result.session.id)
        .collect();
    mixed.sort();
    assert_eq!(mixed, vec!["long".to_string(), "short".to_string()]);

    store.conn.execute_batch("DROP TABLE messages_fts_trigram;").unwrap();
    let legacy = engine.hybrid_search("powercon", None, &filters, 10, 3).unwrap();
    assert_eq!(legacy[0].session.id, "long");
}

fn setup_store() -> Store {
    schema::register_sqlite_vec();
    Store::open_in_memory().unwrap()
}

fn session(id: &str, source: &str, directory: &str) -> Session {
    Session {
        source: source.to_string(),
        source_id: format!("src-{id}"),
        title: id.to_string(),
        directory: Some(directory.to_string()),
        started_at: 1_000,
        updated_at: Some(1_000),
        ..crate::types::test_support::session(id)
    }
}

fn event(seq: u32, kind: &str, target: &str, timestamp: i64) -> RawSessionEvent {
    RawSessionEvent {
        event_seq: seq,
        timestamp: Some(timestamp),
        name: Some(kind.to_string()),
        target: Some(target.to_string()),
        message_seq: Some(1),
        summary: Some(format!("{kind} {target}")),
        attrs_json: Some(r#"{"secret":"nope"}"#.to_string()),
        ..crate::types::test_support::session_event(kind)
    }
}

fn seed_events() -> Store {
    let store = setup_store();
    store.insert_session(&session("s1", "codex", "/tmp/demo")).unwrap();
    store.insert_session(&session("s2", "claude-code", "/tmp/demo")).unwrap();
    store
        .persist_session_events_for_existing_session(
            "codex",
            "src-s1",
            &[
                event(0, "file_write", "/tmp/demo/src/db/schema.rs", 5_000),
                event(1, "command", "/tmp/demo/src/db/schema.rs", 6_000),
                event(2, "file_write", "old_schema.rs", 4_000),
            ],
            1,
            None,
        )
        .unwrap();
    store
        .persist_session_events_for_existing_session(
            "claude-code",
            "src-s2",
            &[event(0, "file_read", "src/db/schema.rs", 8_000)],
            1,
            None,
        )
        .unwrap();
    store
}

fn query_events(
    store: &Store,
    target: &str,
    kinds: Option<&[String]>,
) -> Vec<super::SessionEventHit> {
    SearchEngine::new(&store.conn)
        .list_session_events(&SessionEventQuery {
            kinds,
            target,
            sources: None,
            scope: &ProjectScope::Global,
            limit: 50,
        })
        .unwrap()
}

#[test]
fn list_session_events_matches_exact_or_separator_suffix() {
    let store = seed_events();
    let kinds = vec!["file_write".to_string(), "file_read".to_string()];
    let hits = query_events(&store, "src/db/schema.rs", Some(&kinds));
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].session.id, "s2");
    assert_eq!(hits[0].target.as_deref(), Some("src/db/schema.rs"));
    assert_eq!(hits[1].session.id, "s1");
    assert_eq!(hits[1].target.as_deref(), Some("/tmp/demo/src/db/schema.rs"));
    assert!(hits.iter().all(|hit| hit.kind != "command"));

    let bare = query_events(&store, "schema.rs", Some(&kinds));
    assert_eq!(bare.len(), 2);
    assert!(bare.iter().all(|hit| {
        hit.target.as_deref().is_some_and(|target| {
            target == "schema.rs"
                || target.ends_with("/schema.rs")
                || target.ends_with("\\schema.rs")
        })
    }));

    let no_substring = query_events(&store, "schema.rs", None);
    assert!(no_substring.iter().all(|hit| hit.target.as_deref() != Some("old_schema.rs")));
}

#[test]
fn list_session_events_matches_relative_target_from_absolute_path() {
    let store = seed_events();
    let hits = query_events(&store, "/abs/elsewhere/src/db/schema.rs", None);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session.id, "s2");
    assert_eq!(hits[0].target.as_deref(), Some("src/db/schema.rs"));
}

#[test]
fn list_session_events_orders_newest_event_first() {
    let store = seed_events();
    let hits = query_events(&store, "schema.rs", None);
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].timestamp, Some(8_000));
    assert_eq!(hits[1].timestamp, Some(6_000));
    assert_eq!(hits[1].kind, "command");
    assert_eq!(hits[2].timestamp, Some(5_000));
}

#[test]
fn list_session_events_ranks_timestampless_events_by_session_activity() {
    let store = seed_events();
    let mut recent = session("s3", "cursor", "/tmp/demo");
    recent.updated_at = Some(9_000);
    store.insert_session(&recent).unwrap();
    let mut no_timestamp = event(0, "file_write", "src/db/schema.rs", 0);
    no_timestamp.timestamp = None;
    store
        .persist_session_events_for_existing_session("cursor", "src-s3", &[no_timestamp], 1, None)
        .unwrap();

    let hits = query_events(&store, "schema.rs", None);
    assert_eq!(hits.len(), 4);
    assert_eq!(hits[0].session.id, "s3");
    assert_eq!(hits[0].timestamp, None);
    assert_eq!(hits[1].timestamp, Some(8_000));
}
#[test]
fn message_search_returns_distinct_anchors_and_match_centered_excerpts() {
    crate::db::schema::register_sqlite_vec();
    let store = Store::open_in_memory().unwrap();
    store.conn.execute_batch("INSERT INTO sessions (id, source, source_id, title, started_at) VALUES ('s', 'codex', 'native', 'Test', 1), ('other', 'claude-code', 'other', 'Other', 2);").unwrap();
    let long = format!("{} evidencekeyword fixes the lock", "ordinary preamble ".repeat(100));
    for (id, seq, content) in [
        ("s", 83, long.as_str()),
        ("s", 90, "evidencekeyword verified"),
        ("other", 2, "evidencekeyword unrelated"),
    ] {
        store.conn.execute("INSERT INTO messages (session_id, seq, role, content) VALUES (?1, ?2, 'assistant', ?3)", rusqlite::params![id, seq, content]).unwrap();
    }
    let filters = SearchFilters {
        sources: None,
        time_range: TimeRange::All,
        scope: ProjectScope::Global,
        thread_role: None,
        excluded_session_id: None,
    };
    let engine = SearchEngine::new(&store.conn);
    let hits = engine.search_messages("evidencekeyword", &filters, Some("s"), 10).unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|h| h.session_id == "s"
        && h.role == "assistant"
        && h.excerpt.contains("evidencekeyword")));
    let mut seqs = hits.iter().map(|h| h.seq).collect::<Vec<_>>();
    seqs.sort_unstable();
    assert_eq!(seqs, vec![83, 90]);
    let excluded = SearchFilters { excluded_session_id: Some("s".into()), ..filters.clone() };
    assert_eq!(
        engine.search_messages("evidencekeyword", &excluded, None, 10).unwrap()[0].session_id,
        "other"
    );
    let filtered = SearchFilters { sources: Some(vec!["codex".into()]), ..filters };
    assert_eq!(engine.search_messages("evidencekeyword", &filtered, None, 1).unwrap().len(), 1);
}
#[test]
fn file_history_pages_target_evidence_across_session_projects() {
    use crate::types::{FileEvidence, FileEvidenceKind, FileOperation, FileTarget};
    let store = setup_store();
    let mut foreign = session("foreign", "codex", "/tmp/foreign");
    foreign.repo_remote = Some("github.com/example/foreign".to_string());
    store.insert_session(&foreign).unwrap();
    let mut other = session("other", "codex", "/tmp/project");
    other.repo_remote = Some("github.com/example/project".to_string());
    store.insert_session(&other).unwrap();
    let mut imported = session("import", "unknown-agent", "/tmp/import");
    imported.is_import = true;
    store.insert_session(&imported).unwrap();
    let file = FileEvidence {
        path: "src/lib.rs".to_string(),
        operation: FileOperation::Write,
        kind: FileEvidenceKind::Call,
        cwd: Some("/tmp/project--feature".to_string()),
        target: Some(FileTarget {
            absolute_path: "/tmp/project--feature/src/lib.rs".to_string(),
            repo_root: Some("/tmp/project--feature".to_string()),
            repo_relative_path: Some("src/lib.rs".to_string()),
            repo_remote: Some("github.com/example/project".to_string()),
        }),
    };
    let mut events = (0..65)
        .map(|seq| {
            let mut event = event(seq, "file_write", "src/lib.rs", 5000);
            event.timestamp = (seq < 60).then_some(5000);
            event.source_event_id = Some(format!("native-{seq}"));
            event.files = vec![file.clone()];
            let mut second = file.clone();
            second.path = "src/other.rs".to_string();
            second.target.as_mut().unwrap().repo_relative_path = Some("src/other.rs".to_string());
            second.target.as_mut().unwrap().absolute_path =
                "/tmp/project--feature/src/other.rs".to_string();
            event.files.push(second);
            event
        })
        .collect::<Vec<_>>();
    let mut command = event(65, "command", "edit src/lib.rs", 5000);
    command.files = vec![file.clone()];
    command.files[0].kind = FileEvidenceKind::Command;
    command.command_evidence_status = Some(crate::types::CommandEvidenceStatus::Complete);
    events.push(command);
    let mut unknown = event(66, "command", "edit src/lib.rs", 5000);
    unknown.files = vec![file.clone()];
    unknown.files[0].kind = FileEvidenceKind::Command;
    unknown.files[0].target = None;
    unknown.files[0].cwd = None;
    events.push(unknown);
    let mut approval = event(67, "approval", "src/lib.rs", 5000);
    approval.files = vec![file.clone()];
    approval.status = Some("approved".to_string());
    events.push(approval);
    let mut observed = event(68, "tool_result", "src/lib.rs", 5000);
    observed.files = vec![file.clone()];
    observed.files[0].kind = FileEvidenceKind::Observation;
    observed.files[0].operation = FileOperation::Delete;
    events.push(observed);
    let mut native_absolute = event(69, "command", "edit src/lib.rs", 5000);
    native_absolute.files = vec![file.clone()];
    native_absolute.files[0].kind = FileEvidenceKind::Command;
    native_absolute.files[0].path = "/tmp/project--feature/src/lib.rs".to_string();
    native_absolute.files[0].target = None;
    native_absolute.files[0].cwd = None;
    events.push(native_absolute);
    store
        .persist_session_events_for_existing_session("codex", "src-foreign", &events, 1, None)
        .unwrap();
    let mut wrong = event(0, "file_write", "src/lib.rs", 5000);
    wrong.files = vec![file];
    wrong.files[0].target.as_mut().unwrap().repo_remote =
        Some("github.com/other/project".to_string());
    wrong.files[0].target.as_mut().unwrap().repo_root = Some("/tmp/other-project".to_string());
    wrong.files[0].target.as_mut().unwrap().absolute_path =
        "/tmp/other-project/src/lib.rs".to_string();
    store
        .persist_session_events_for_existing_session(
            "codex",
            "src-other",
            &[wrong.clone()],
            1,
            None,
        )
        .unwrap();
    let engine = SearchEngine::new(&store.conn);
    let query = FileHistoryQuery {
        target: engine
            .resolve_file_history_target("https://github.com/example/project.git", "src/lib.rs")
            .unwrap(),
        sources: None,
        kind: None,
        include_command_candidates: false,
    };
    assert!(engine.resolve_file_history_target("project", "src/lib.rs").is_err());
    assert!(engine.resolve_file_history_target("project--feature", "src/lib.rs").is_err());
    let basename = FileHistoryQuery {
        target: engine.resolve_file_history_target("github.com/example/project", "lib.rs").unwrap(),
        ..query.clone()
    };
    assert!(engine.file_history_page(&basename, 20, None).unwrap().events.is_empty());
    let first = engine.file_history_page(&query, 17, None).unwrap();
    assert_eq!(first.events.len(), 17);
    assert!(first.events.iter().all(|event| event.hit.session.id == "foreign"));
    let write = first.events.iter().find(|event| event.hit.kind == "file_write").unwrap();
    assert_eq!(write.evidence.file_associations, 2);
    assert_eq!(write.evidence.files.len(), 1);
    assert!(first.events.iter().any(|event| event.hit.kind == "approval"
        && event.evidence.status.as_deref() == Some("approved")));
    let imported = first
        .coverage
        .as_ref()
        .unwrap()
        .sources
        .iter()
        .find(|source| source.source == "unknown-agent")
        .unwrap();
    assert!(!imported.registered);
    assert_eq!(imported.imported_sessions, 1);
    assert_eq!(imported.sessions_without_parser_state, 1);
    let cursor = first.next_cursor.clone().unwrap();
    store
        .persist_session_events_for_existing_session("codex", "src-other", &[wrong], 1, Some(9999))
        .unwrap();
    assert!(engine.file_history_page(&query, 17, Some(&cursor)).is_ok());
    let mut references =
        first.events.iter().map(|hit| hit.evidence.event_ref.clone()).collect::<Vec<_>>();
    let mut next = first.next_cursor;
    let mut last_unknown = false;
    while let Some(cursor) = next {
        let page = engine.file_history_page(&query, 17, Some(&cursor)).unwrap();
        assert!(page.coverage.is_none());
        for hit in &page.events {
            if last_unknown {
                assert!(hit.hit.timestamp.is_none());
            }
            last_unknown = hit.hit.timestamp.is_none();
            references.push(hit.evidence.event_ref.clone());
        }
        next = page.next_cursor;
    }
    assert_eq!(references.len(), 67);
    assert_eq!(references.iter().collect::<std::collections::HashSet<_>>().len(), 67);
    let candidates = FileHistoryQuery { include_command_candidates: true, ..query.clone() };
    let mut count = 0;
    let mut next = None;
    loop {
        let page = engine.file_history_page(&candidates, 50, next.as_deref()).unwrap();
        count += page.events.len();
        next = page.next_cursor;
        if next.is_none() {
            break;
        }
    }
    assert_eq!(count, 68);
    let absolute = FileHistoryQuery {
        target: engine
            .resolve_file_history_target(
                "https://github.com/example/project.git",
                "/tmp/project--feature/src/lib.rs",
            )
            .unwrap(),
        ..candidates.clone()
    };
    let first_absolute = engine.file_history_page(&absolute, 50, None).unwrap();
    assert!(first_absolute.events.iter().any(|hit| hit.hit.event_seq == 69));
    assert!(first_absolute.events.iter().all(|hit| hit.hit.event_seq != 66));
    let unresolved_absolute = FileHistoryQuery {
        target: engine
            .resolve_file_history_target(
                "https://github.com/example/unrelated.git",
                "/tmp/project--feature/src/lib.rs",
            )
            .unwrap(),
        ..absolute.clone()
    };
    assert!(unresolved_absolute.target.path.is_none());
    let native_matches = engine.file_history_page(&unresolved_absolute, 50, None).unwrap();
    assert!(native_matches.events.iter().any(|hit| hit.hit.event_seq == 69));
    assert!(native_matches.events.iter().all(|hit| hit.hit.event_seq != 66));
    assert!(engine.file_history_page(&candidates, 17, Some(&cursor)).is_err());
    events[0].files[0].operation = FileOperation::Delete;
    store
        .persist_session_events_for_existing_session("codex", "src-foreign", &events, 1, None)
        .unwrap();
    assert!(
        engine
            .file_history_page(&query, 17, Some(&cursor))
            .unwrap_err()
            .to_string()
            .contains("stale")
    );
    store
        .conn
        .execute("UPDATE sessions SET summary = ?1 WHERE id = 'foreign'", ["x".repeat(131072)])
        .unwrap();
    let first = engine.file_history_page(&query, 1, None).unwrap();
    let second = engine.file_history_page(&query, 1, first.next_cursor.as_deref()).unwrap();
    assert_eq!(first.events.len(), 1);
    assert_eq!(second.events.len(), 1);
    assert_ne!(first.events[0].evidence.event_ref, second.events[0].evidence.event_ref);
    store.conn.execute("UPDATE event_files SET evidence_json = json_set(evidence_json, '$.cwd', printf('%.*c', 67108865, 'x')) WHERE event_id = (SELECT MIN(id) FROM session_events WHERE session_id = 'foreign') AND position = 1", []).unwrap();
    assert_eq!(engine.file_history_page(&query, 1, None).unwrap().events.len(), 1);
    let before_delete = engine.file_history_page(&query, 1, None).unwrap();
    store.delete_session_data("codex", "src-foreign").unwrap();
    assert!(engine.file_history_page(&query, 1, before_delete.next_cursor.as_deref()).is_err());
}

#[test]
fn event_references_bind_an_index_and_an_immutable_record() {
    let stores = [setup_store(), setup_store()];
    let mut references = Vec::new();
    for store in &stores {
        store.insert_session(&session("s1", "codex", "/tmp/project")).unwrap();
        store
            .persist_session_events_for_existing_session(
                "codex",
                "src-s1",
                &[event(0, "file_write", "src/lib.rs", 5000)],
                1,
                None,
            )
            .unwrap();
        let tx = store.conn.unchecked_transaction().unwrap();
        references.push(crate::db::event_store::event_reference(&tx, 1).unwrap());
    }
    assert_eq!(references[0].event_id, references[1].event_id);
    assert_ne!(references[0], references[1]);
    let store = &stores[0];
    store
        .persist_session_events_for_existing_session(
            "codex",
            "src-s1",
            &[event(0, "file_write", "src/lib.rs", 5000)],
            1,
            None,
        )
        .unwrap();
    let tx = store.conn.unchecked_transaction().unwrap();
    assert!(crate::db::event_store::event_reference(&tx, references[0].event_id).is_err());
    let id = tx.query_row("SELECT id FROM session_events", [], |row| row.get(0)).unwrap();
    let replacement = crate::db::event_store::event_reference(&tx, id).unwrap();
    assert_eq!(replacement.index_id, references[0].index_id);
    assert!(replacement.event_id > references[0].event_id);
}
