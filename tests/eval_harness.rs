use recall::bench::{EvalEntry, ExpectedSession, evaluate};
use recall::db::schema;
use recall::db::search::SearchEngine;
use recall::db::store::Store;
use recall::types::{Message, Role, Session};

fn setup() -> Store {
    schema::register_sqlite_vec();
    Store::open_in_memory().unwrap()
}

fn session(id: &str, source: &str, source_id: &str, title: &str) -> Session {
    Session {
        id: id.to_string(),
        source: source.to_string(),
        source_id: source_id.to_string(),
        title: title.to_string(),
        directory: None,
        started_at: chrono::Utc::now().timestamp_millis(),
        updated_at: None,
        message_count: 1,
        entrypoint: None,
    }
}

fn message(session_id: &str, content: &str, seq: u32) -> Message {
    Message {
        session_id: session_id.to_string(),
        role: Role::User,
        content: content.to_string(),
        timestamp: Some(chrono::Utc::now().timestamp_millis()),
        seq,
    }
}

fn expected(source: &str, source_id: &str) -> ExpectedSession {
    ExpectedSession { source: source.to_string(), source_id: source_id.to_string() }
}

fn entry(query: &str, expected: Vec<ExpectedSession>) -> EvalEntry {
    EvalEntry { query: query.to_string(), expected, notes: None }
}

fn seed_corpus(store: &Store) {
    store.insert_session(&session("s1", "claude-code", "cc-1", "Rust async debugging")).unwrap();
    store.insert_session(&session("s2", "codex", "cdx-1", "Ratatui widget layout")).unwrap();
    store.insert_session(&session("s3", "opencode", "oc-1", "SQL migration rollback")).unwrap();

    store
        .insert_messages(&[
            message("s1", "how do I debug tokio async streams with backpressure", 0),
            message("s2", "ratatui constraint layout tricks for nested panels", 0),
            message("s3", "postgres migration rollback strategies with zero downtime", 0),
        ])
        .unwrap();
}

#[test]
fn evaluate_perfect_hit_at_rank_one() {
    let store = setup();
    seed_corpus(&store);
    let engine = SearchEngine::new(&store.conn);

    let entries = vec![entry("tokio async streams", vec![expected("claude-code", "cc-1")])];

    let report = evaluate(&engine, &entries, |_| None, 20).unwrap();

    assert_eq!(report.total, 1);
    assert_eq!(report.hit_at_5, 1);
    assert_eq!(report.hit_at_10, 1);
    assert!((report.mrr() - 1.0).abs() < 1e-9);
    assert!(report.failures.is_empty());
}

#[test]
fn evaluate_miss_when_expected_not_in_corpus() {
    let store = setup();
    seed_corpus(&store);
    let engine = SearchEngine::new(&store.conn);

    let entries = vec![entry("postgres rollback", vec![expected("claude-code", "ghost-id")])];

    let report = evaluate(&engine, &entries, |_| None, 20).unwrap();

    assert_eq!(report.total, 1);
    assert_eq!(report.hit_at_5, 0);
    assert_eq!(report.hit_at_10, 0);
    assert!(report.mrr().abs() < 1e-9);
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].rank, None);
}

#[test]
fn evaluate_multi_expected_picks_best_rank() {
    let store = setup();
    seed_corpus(&store);
    let engine = SearchEngine::new(&store.conn);

    let entries = vec![entry(
        "ratatui layout",
        vec![expected("claude-code", "ghost-id"), expected("codex", "cdx-1")],
    )];

    let report = evaluate(&engine, &entries, |_| None, 20).unwrap();

    assert_eq!(report.hit_at_5, 1);
    assert!(report.mrr() > 0.0);
}

#[test]
fn evaluate_mixed_hit_and_miss() {
    let store = setup();
    seed_corpus(&store);
    let engine = SearchEngine::new(&store.conn);

    let entries = vec![
        entry("tokio async streams", vec![expected("claude-code", "cc-1")]),
        entry("unrelated xyzzy plugh", vec![expected("codex", "cdx-1")]),
    ];

    let report = evaluate(&engine, &entries, |_| None, 20).unwrap();

    assert_eq!(report.total, 2);
    assert_eq!(report.hit_at_5, 1);
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].rank, None);
}

#[test]
fn evaluate_empty_query_is_a_miss_not_a_crash() {
    let store = setup();
    seed_corpus(&store);
    let engine = SearchEngine::new(&store.conn);

    let entries = vec![entry("", vec![expected("claude-code", "cc-1")])];

    let report = evaluate(&engine, &entries, |_| None, 20).unwrap();
    assert_eq!(report.hit_at_5, 0);
    assert_eq!(report.failures.len(), 1);
}
