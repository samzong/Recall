use super::{SearchEngine, SearchFilters, TimeRange};
use crate::db::{schema, store::Store};
use crate::project_scope::ProjectScope;
use crate::types::{Role, test_support};

fn seed(contents: &[(&str, &str, &str)]) -> Store {
    schema::register_sqlite_vec();
    let store = Store::open_in_memory().unwrap();
    for &(id, source, content) in contents {
        let mut session = test_support::session(id);
        session.source = source.into();
        session.source_id = id.into();
        store.insert_session(&session).unwrap();
        store.insert_messages(&[test_support::message(id, Role::User, content, 0)]).unwrap();
    }
    store
}

fn filters(source: Option<&str>) -> SearchFilters {
    SearchFilters {
        sources: source.map(|source| vec![source.into()]),
        time_range: TimeRange::All,
        scope: ProjectScope::Global,
        thread_role: None,
        excluded_session_id: None,
    }
}

fn assert_matches(store: &Store, query: &str, source: Option<&str>, expected: &[&str]) {
    let engine = SearchEngine::new(&store.conn);
    let filters = filters(source);
    let mut messages = engine
        .search_messages(query, &filters, None, 50)
        .unwrap()
        .into_iter()
        .map(|hit| hit.session_id)
        .collect::<Vec<_>>();
    messages.sort();
    assert_eq!(messages, expected, "message query: {query:?}, source: {source:?}");
    let mut sessions = engine
        .hybrid_search(query, None, &filters, 50, 3)
        .unwrap()
        .into_iter()
        .map(|hit| hit.session.id)
        .collect::<Vec<_>>();
    sessions.sort();
    assert_eq!(sessions, expected, "session query: {query:?}, source: {source:?}");
    let mut page = engine
        .hybrid_search_page(query, None, &filters, None, 0)
        .unwrap()
        .into_iter()
        .map(|hit| hit.session.id)
        .collect::<Vec<_>>();
    page.sort();
    assert_eq!(page, expected, "session page query: {query:?}, source: {source:?}");
}

#[test]
fn hyphenated_message_search_recalls_adjacent_words() {
    let store = seed(&[("hyphen", "claude-code", "incremental-witness")]);
    let hits = SearchEngine::new(&store.conn)
        .search_messages("incremental-witness", &filters(None), None, 10)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session_id, "hyphen");
}

#[test]
fn hyphenated_session_search_recalls_adjacent_words() {
    let store = seed(&[("hyphen", "claude-code", "incremental-witness")]);
    let hits = SearchEngine::new(&store.conn)
        .hybrid_search("incremental-witness", None, &filters(None), 10, 3)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session.id, "hyphen");
}

fn control_character_fixture() -> Store {
    seed(&[
        ("first", "codex", "incremental"),
        ("last", "codex", "witness"),
        ("phrase", "codex", "incremental-witness"),
        ("joined", "codex", "incrementalwitness"),
    ])
}

#[test]
fn control_characters_separate_message_query_terms() {
    let store = control_character_fixture();
    let engine = SearchEngine::new(&store.conn);
    for query in ["incremental witness", "incremental\0witness", "incremental\u{1}witness"] {
        let mut ids = engine
            .search_messages(query, &filters(None), None, 10)
            .unwrap()
            .into_iter()
            .map(|hit| hit.session_id)
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(ids, ["first", "last", "phrase"], "query: {query:?}");
    }
}

#[test]
fn control_characters_separate_session_query_terms() {
    let store = control_character_fixture();
    let engine = SearchEngine::new(&store.conn);
    for query in ["incremental witness", "incremental\0witness", "incremental\u{1}witness"] {
        let mut ids = engine
            .hybrid_search(query, None, &filters(None), 10, 3)
            .unwrap()
            .into_iter()
            .map(|hit| hit.session.id)
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(ids, ["first", "last", "phrase"], "query: {query:?}");
    }
}

#[test]
fn punctuation_keeps_adjacency_prefixes_and_source_filters() {
    let store = seed(&[
        ("hyphen", "claude-code", "incremental-witness"),
        ("space", "codex", "incremental witness"),
        ("longer", "codex", "incremental witnesses"),
        ("first", "claude-code", "incremental only"),
        ("last", "codex", "witness only"),
        ("gap", "codex", "incremental unrelated witness"),
        ("reverse", "codex", "witness incremental"),
        ("joined", "codex", "incrementalwitness"),
        ("other", "codex", "fallback"),
    ]);
    for query in [
        "incremental-witness",
        "incremental-wit",
        "incremental.witness",
        "incremental/witness",
        "incremental—witness",
        "\"incremental-witness\"",
    ] {
        assert_matches(&store, query, None, &["hyphen", "longer", "space"]);
        assert_matches(&store, query, Some("claude-code"), &["hyphen"]);
        assert_matches(&store, query, Some("codex"), &["longer", "space"]);
    }
    assert_matches(&store, "incremental-witness absent", None, &["hyphen", "space"]);
    assert_matches(&store, "incremental-witness fallback", None, &["hyphen", "other", "space"]);
    assert_matches(
        &store,
        "incremental witness",
        None,
        &["first", "gap", "hyphen", "last", "longer", "reverse", "space"],
    );
}

#[test]
fn punctuation_search_handles_identifiers_paths_quotes_and_empty_terms() {
    let store = seed(&[
        ("identifier", "codex", "foo_bar"),
        ("identifier-space", "codex", "foo bar"),
        ("path", "codex", "src/db/search.rs"),
        ("path-gap", "codex", "src/db/other/search.rs"),
        ("quote", "codex", "alpha\"beta"),
        ("one", "codex", "x"),
        ("long", "codex", "xyz"),
    ]);
    assert_matches(&store, "foo_bar", None, &["identifier", "identifier-space"]);
    assert_matches(&store, "src/db/search.rs", None, &["path"]);
    assert_matches(&store, "alpha\"beta", None, &["quote"]);
    assert_matches(&store, "\"x\"", None, &["one"]);
    for query in ["", "---", "_", "\" OR \"", "... / : * ()"] {
        assert_matches(&store, query, None, &[]);
    }
}

#[test]
fn punctuation_search_keeps_the_existing_last_term_prefix_rule() {
    let store =
        seed(&[("short", "codex", "foo-a v1.2.x"), ("long", "codex", "foo-apple v1.2.xyz")]);
    assert_matches(&store, "foo_a", None, &["long", "short"]);
    assert_matches(&store, "foo-a", None, &["long", "short"]);
    assert_matches(&store, "v1.2.x", None, &["long", "short"]);
    assert_matches(&store, "foo-ap", None, &["long"]);
}

#[test]
fn punctuation_search_preserves_trigram_substrings_and_unicode61_fallback() {
    let store = seed(&[
        ("hyphen", "claude-code", "统计-不准确"),
        ("joined", "codex", "统计不准确"),
        ("space", "codex", "统计 不准确"),
        ("short", "codex", "矩阵运算"),
        ("mixed", "codex", "缓存-cache"),
    ]);
    assert_matches(&store, "统计-不准确", None, &["hyphen"]);
    assert_matches(&store, "\"统计-不准确\"", Some("claude-code"), &["hyphen"]);
    assert_matches(&store, "计不准", None, &["joined"]);
    assert_matches(&store, "矩阵", None, &["short"]);
    assert_matches(&store, "缓存-cache", None, &["mixed"]);
    store.conn.execute_batch("DROP TABLE messages_fts_trigram;").unwrap();
    assert_matches(&store, "统计-不准确", None, &["hyphen", "space"]);
    assert_matches(&store, "缓存-cache", None, &["mixed"]);
}
