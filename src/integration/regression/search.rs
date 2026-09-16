use super::*;

#[test]
fn fts_search_basic() {
    let store = setup();
    let session = make_session("s1", "test", "raw1", "Rust programming");
    store.insert_session(&session).unwrap();

    let messages = vec![make_message("s1", Role::User, "how do I use iterators in Rust", 0)];
    store.insert_messages(&messages).unwrap();

    let engine = SearchEngine::new(&store.conn);
    let results = engine.hybrid_search("iterators", None, &no_filters(), 10, 3).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].session.id, "s1");
}

#[test]
fn fts_search_no_results() {
    let store = setup();
    let session = make_session("s1", "test", "raw1", "Test");
    store.insert_session(&session).unwrap();

    let messages = vec![make_message("s1", Role::User, "hello world", 0)];
    store.insert_messages(&messages).unwrap();

    let engine = SearchEngine::new(&store.conn);
    let results = engine.hybrid_search("zzzznonexistent", None, &no_filters(), 10, 3).unwrap();
    assert!(results.is_empty());
}

#[test]
fn fts_search_empty_query() {
    let store = setup();
    let engine = SearchEngine::new(&store.conn);
    let results = engine.hybrid_search("", None, &no_filters(), 10, 3).unwrap();
    assert!(results.is_empty());
}

#[test]
fn fts_search_special_characters() {
    let store = setup();
    let session = make_session("s1", "test", "raw1", "Test");
    store.insert_session(&session).unwrap();

    let messages = vec![make_message("s1", Role::User, "fix the bug in parser", 0)];
    store.insert_messages(&messages).unwrap();

    let engine = SearchEngine::new(&store.conn);
    let results = engine.hybrid_search("bug OR 1=1 --", None, &no_filters(), 10, 3).unwrap();
    assert!(!results.is_empty());
}

#[test]
fn fts_search_sql_keywords_safe() {
    let store = setup();
    let session = make_session("s1", "test", "raw1", "Test");
    store.insert_session(&session).unwrap();

    let messages = vec![make_message("s1", Role::User, "AND OR NOT NEAR", 0)];
    store.insert_messages(&messages).unwrap();

    let engine = SearchEngine::new(&store.conn);
    let result = engine.hybrid_search("AND OR NOT", None, &no_filters(), 10, 3);
    assert!(result.is_ok(), "FTS5 keywords must not cause SQL errors");
}

#[test]
fn fts_search_keeps_partial_matches_when_full_match_exists() {
    let store = setup();
    store.insert_session(&make_session("both", "test", "both", "Both")).unwrap();
    store.insert_session(&make_session("partial", "test", "partial", "Partial")).unwrap();
    store
        .insert_messages(&[
            make_message("both", Role::User, "debug tokio runtime", 0),
            make_message("both", Role::Assistant, "look at streams backpressure", 1),
            make_message("partial", Role::User, "tokio parser", 0),
        ])
        .unwrap();

    let engine = SearchEngine::new(&store.conn);
    let mut ids: Vec<_> = engine
        .hybrid_search("tokio streams", None, &no_filters(), 10, 3)
        .unwrap()
        .into_iter()
        .map(|result| result.session.id)
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["both".to_string(), "partial".to_string()]);
}

#[test]
fn fts_search_matches_any_term_without_full_match() {
    let store = setup();
    store.insert_session(&make_session("alpha", "test", "alpha", "Alpha")).unwrap();
    store.insert_session(&make_session("beta", "test", "beta", "Beta")).unwrap();
    store
        .insert_messages(&[
            make_message("alpha", Role::User, "only tokio here", 0),
            make_message("beta", Role::User, "only streams here", 0),
        ])
        .unwrap();

    let engine = SearchEngine::new(&store.conn);
    let mut ids: Vec<_> = engine
        .hybrid_search("tokio streams", None, &no_filters(), 10, 3)
        .unwrap()
        .into_iter()
        .map(|r| r.session.id)
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["alpha".to_string(), "beta".to_string()]);
}

#[test]
fn fts_search_prefixes_last_token() {
    let store = setup();
    store.insert_session(&make_session("s1", "test", "s1", "Power")).unwrap();
    store
        .insert_messages(&[make_message("s1", Role::User, "enable powercontext backfill", 0)])
        .unwrap();

    let engine = SearchEngine::new(&store.conn);
    let results = engine.hybrid_search("powercon", None, &no_filters(), 10, 3).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].session.id, "s1");
}

#[test]
fn hybrid_search_fts_only_without_embedding() {
    let store = setup();
    let session = make_session("s1", "test", "raw1", "Debugging session");
    store.insert_session(&session).unwrap();

    let messages = vec![make_message("s1", Role::User, "segfault in main loop", 0)];
    store.insert_messages(&messages).unwrap();

    let engine = SearchEngine::new(&store.conn);
    let results = engine.hybrid_search("segfault", None, &no_filters(), 10, 3).unwrap();
    assert_eq!(results.len(), 1);
}

fn seed_semantic_boundary_sessions(store: &Store, count: usize) {
    store
        .conn
        .execute_batch(&format!(
            "WITH RECURSIVE seq(n) AS (
                 SELECT 0
                 UNION ALL
                 SELECT n + 1 FROM seq WHERE n + 1 < {count}
             )
             INSERT INTO sessions (id, source, source_id, title, started_at, message_count)
             SELECT printf('semantic-%05d', n), 'test', printf('raw-%05d', n),
                    printf('Semantic session %05d', n), n, 1
             FROM seq;
             INSERT INTO messages (session_id, role, content, timestamp, seq)
             SELECT id, 'user', 'semanticboundary ' || id, started_at, 0
             FROM sessions
             WHERE source = 'test';"
        ))
        .unwrap();
}

fn add_semantic_boundary_embedding(store: &Store) -> Vec<f32> {
    let message_id: i64 = store
        .conn
        .query_row("SELECT id FROM messages ORDER BY id LIMIT 1", [], |row| row.get(0))
        .unwrap();
    let embedding = vec![0.1f32; 384];
    store.upsert_embeddings(&[(message_id, &embedding)]).unwrap();
    embedding
}

fn seed_semantic_page_fixture(store: &Store) -> Vec<f32> {
    for index in 0..6 {
        let id = format!("semantic-fts-{index:02}");
        let session = make_session(&id, "test", &format!("raw-fts-{index:02}"), "Semantic FTS");
        store.insert_session(&session).unwrap();
        store.insert_messages(&[make_message(&id, Role::User, "semanticstable", 0)]).unwrap();
    }

    let mut filler =
        make_session("semantic-vec-fill", "test", "raw-vec-fill", "Semantic vector filler");
    filler.message_count = 30;
    store.insert_session(&filler).unwrap();
    let filler_messages = (0..30)
        .map(|seq| make_message("semantic-vec-fill", Role::User, "semantic filler", seq))
        .collect::<Vec<_>>();
    store.insert_messages(&filler_messages).unwrap();

    let mut stmt = store
        .conn
        .prepare("SELECT id FROM messages WHERE session_id = 'semantic-vec-fill' ORDER BY seq")
        .unwrap();
    let mut message_ids = stmt
        .query_map([], |row| row.get::<_, i64>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    message_ids.push(
        store
            .conn
            .query_row("SELECT id FROM messages WHERE session_id = 'semantic-fts-04'", [], |row| {
                row.get(0)
            })
            .unwrap(),
    );

    let vectors = (1..=message_ids.len())
        .map(|rank| {
            let mut vector = vec![0.0f32; 384];
            vector[0] = rank as f32 / 1_000.0;
            vector
        })
        .collect::<Vec<_>>();
    let embeddings = message_ids
        .iter()
        .zip(&vectors)
        .map(|(&message_id, vector)| (message_id, vector.as_slice()))
        .collect::<Vec<_>>();
    store.upsert_embeddings(&embeddings).unwrap();

    vec![0.0f32; 384]
}

#[test]
fn semantic_query_crosses_sqlite_vec_boundary() {
    let store = setup();
    seed_semantic_boundary_sessions(&store, 300);
    let embedding = add_semantic_boundary_embedding(&store);
    let engine = SearchEngine::new(&store.conn);

    let results =
        engine.hybrid_search("semanticboundary", Some(&embedding), &no_filters(), 274, 3).unwrap();
    assert_eq!(results.len(), 274);

    let page = engine
        .hybrid_search_page("semanticboundary", Some(&embedding), &no_filters(), Some(50), 224)
        .unwrap();
    assert_eq!(page.len(), 50);
}

#[test]
fn semantic_adjacent_pages_follow_one_global_order() {
    let store = setup();
    let embedding = seed_semantic_page_fixture(&store);
    let engine = SearchEngine::new(&store.conn);

    let first_page = engine
        .hybrid_search_page("semanticstable", Some(&embedding), &no_filters(), Some(2), 0)
        .unwrap();
    let second_page = engine
        .hybrid_search_page("semanticstable", Some(&embedding), &no_filters(), Some(2), 2)
        .unwrap();
    let global = engine
        .hybrid_search_page("semanticstable", Some(&embedding), &no_filters(), None, 0)
        .unwrap();

    let paged_ids = first_page
        .iter()
        .chain(&second_page)
        .map(|result| result.session.id.as_str())
        .collect::<Vec<_>>();
    let global_prefix =
        global.iter().take(4).map(|result| result.session.id.as_str()).collect::<Vec<_>>();

    assert_eq!(global_prefix[0], "semantic-fts-04");
    assert_eq!(paged_ids, global_prefix);
}

#[test]
fn semantic_search_excludes_a_session_before_vector_and_hybrid_limits() {
    let store = setup();
    let embedding = seed_semantic_page_fixture(&store);
    let engine = SearchEngine::new(&store.conn);

    let mut vector_filters = no_filters();
    vector_filters.excluded_session_id = Some("semantic-vec-fill".to_string());
    let vector =
        engine.hybrid_search("lexically-absent", Some(&embedding), &vector_filters, 1, 3).unwrap();
    assert_eq!(vector.len(), 1);
    assert_eq!(vector[0].session.id, "semantic-fts-04");

    let mut hybrid_filters = no_filters();
    hybrid_filters.excluded_session_id = Some("semantic-fts-04".to_string());
    let hybrid =
        engine.hybrid_search("semanticstable", Some(&embedding), &hybrid_filters, 2, 3).unwrap();
    assert_eq!(hybrid.len(), 2);
    assert!(hybrid.iter().all(|result| result.session.id != "semantic-fts-04"));
}

#[test]
fn semantic_query_all_returns_complete_fts_set() {
    let store = setup();
    seed_semantic_boundary_sessions(&store, 10_001);
    let engine = SearchEngine::new(&store.conn);

    let text_results =
        engine.hybrid_search_page("semanticboundary", None, &no_filters(), None, 0).unwrap();
    assert_eq!(text_results.len(), 10_001);
    assert!(text_results.iter().all(|result| {
        result.snippet.as_deref().and_then(|snippet| snippet.strip_prefix("semanticboundary "))
            == Some(result.session.id.as_str())
    }));

    let embedding = add_semantic_boundary_embedding(&store);
    let semantic_results = engine
        .hybrid_search_page("semanticboundary", Some(&embedding), &no_filters(), None, 0)
        .unwrap();
    assert_eq!(semantic_results.len(), 10_001);
}

#[test]
fn semantic_search_arithmetic_is_saturating() {
    let store = setup();
    let engine = SearchEngine::new(&store.conn);
    let embedding = vec![0.1f32; 384];

    let direct = engine
        .hybrid_search("semanticboundary", Some(&embedding), &no_filters(), usize::MAX, usize::MAX)
        .unwrap();
    assert!(direct.is_empty());

    let page = engine
        .hybrid_search_page(
            "semanticboundary",
            Some(&embedding),
            &no_filters(),
            Some(usize::MAX),
            usize::MAX,
        )
        .unwrap();
    assert!(page.is_empty());
}

#[test]
fn search_with_source_filter() {
    let store = setup();
    let s1 = make_session("s1", "claude-code", "raw1", "Claude session");
    let s2 = make_session("s2", "opencode", "raw2", "OpenCode session");
    store.insert_session(&s1).unwrap();
    store.insert_session(&s2).unwrap();

    let messages = vec![
        make_message("s1", Role::User, "fix the parser", 0),
        make_message("s2", Role::User, "fix the parser", 0),
    ];
    store.insert_messages(&messages).unwrap();

    let engine = SearchEngine::new(&store.conn);
    let filters = SearchFilters {
        sources: Some(vec!["claude-code".to_string()]),
        time_range: TimeRange::All,
        scope: ProjectScope::Global,
        thread_role: None,
        excluded_session_id: None,
    };
    let results = engine.hybrid_search("parser", None, &filters, 10, 3).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].session.source, "claude-code");
}

#[test]
fn search_surfaces_subagent_content_when_parent_does_not_match() {
    let store = setup();
    store.insert_session(&make_session("parent", "codex", "P", "Primary")).unwrap();
    store.insert_session(&make_session("child", "codex", "C", "Subagent")).unwrap();
    store
        .conn
        .execute("UPDATE sessions SET thread_role = 'subagent' WHERE id = 'child'", [])
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO session_parent_links
                 (session_id, relation, parent_source, parent_source_id)
             VALUES ('child', 'spawn', 'codex', 'P')",
            [],
        )
        .unwrap();
    let messages = vec![
        make_message("parent", Role::User, "set up deployment", 0),
        make_message("child", Role::User, "investigate the flaky wombat test", 0),
    ];
    store.insert_messages(&messages).unwrap();

    let engine = SearchEngine::new(&store.conn);
    let results = engine.hybrid_search("wombat", None, &no_filters(), 10, 3).unwrap();
    let ids: Vec<String> = results.into_iter().map(|result| result.session.id).collect();
    assert_eq!(ids, vec!["child".to_string()], "subagent content stays searchable");
}

#[test]
fn search_with_directory_filter_respects_project_boundary() {
    let store = setup();
    let mut exact = make_session("s1", "codex", "raw1", "Exact project");
    exact.directory = Some("/tmp/project".to_string());
    let mut child = make_session("s2", "opencode", "raw2", "Child project path");
    child.directory = Some("/tmp/project/subdir".to_string());
    let mut sibling = make_session("s3", "claude-code", "raw3", "Sibling prefix");
    sibling.directory = Some("/tmp/project-sibling".to_string());
    let mut missing = make_session("s4", "gemini-cli", "raw4", "Missing directory");
    missing.directory = None;

    for session in [&exact, &child, &sibling, &missing] {
        store.insert_session(session).unwrap();
    }
    let messages = vec![
        make_message("s1", Role::User, "fix the parser", 0),
        make_message("s2", Role::User, "fix the parser", 0),
        make_message("s3", Role::User, "fix the parser", 0),
        make_message("s4", Role::User, "fix the parser", 0),
    ];
    store.insert_messages(&messages).unwrap();

    let engine = SearchEngine::new(&store.conn);
    let filters = SearchFilters {
        sources: None,
        time_range: TimeRange::All,
        scope: ProjectScope::Directory("/tmp/project".to_string()),
        thread_role: None,
        excluded_session_id: None,
    };
    let results = engine.hybrid_search("parser", None, &filters, 10, 3).unwrap();
    let mut ids: Vec<String> = results.into_iter().map(|result| result.session.id).collect();
    ids.sort();

    assert_eq!(ids, vec!["s1".to_string(), "s2".to_string()]);
}

#[test]
fn recent_sessions_with_directory_filter_respects_project_boundary() {
    let store = setup();
    let mut exact = make_session("s1", "codex", "raw1", "Exact project");
    exact.directory = Some("/tmp/project".to_string());
    let mut sibling = make_session("s2", "opencode", "raw2", "Sibling prefix");
    sibling.directory = Some("/tmp/project-sibling".to_string());

    store.insert_session(&exact).unwrap();
    store.insert_session(&sibling).unwrap();

    let sessions = store
        .list_recent_sessions_for_search_scope(
            None,
            TimeRange::All,
            &ProjectScope::Directory("/tmp/project".to_string()),
            None,
            10,
        )
        .unwrap();

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "s1");
}

#[test]
fn search_with_repo_filter_matches_sibling_worktrees() {
    let store = setup();
    let mut main = make_session("s1", "codex", "raw1", "Main worktree");
    main.directory = Some("/tmp/Recall".to_string());
    main.repo_remote = Some("github.com/samzong/Recall".to_string());
    main.repo_slug = Some("samzong/Recall".to_string());
    main.repo_name = Some("Recall".to_string());
    let mut sibling = make_session("s2", "opencode", "raw2", "Sibling worktree");
    sibling.directory = Some("/tmp/Recall--feature".to_string());
    sibling.repo_remote = Some("github.com/samzong/Recall".to_string());
    sibling.repo_slug = Some("samzong/Recall".to_string());
    sibling.repo_name = Some("Recall".to_string());
    let mut other = make_session("s3", "claude-code", "raw3", "Other repo");
    other.directory = Some("/tmp/other".to_string());
    other.repo_remote = Some("github.com/other/Recall".to_string());
    other.repo_slug = Some("other/Recall".to_string());
    other.repo_name = Some("Recall".to_string());

    for session in [&main, &sibling, &other] {
        store.insert_session(session).unwrap();
        store.insert_messages(&[make_message(&session.id, Role::User, "fix parser", 0)]).unwrap();
    }

    let engine = SearchEngine::new(&store.conn);
    let filters = SearchFilters {
        sources: None,
        time_range: TimeRange::All,
        scope: ProjectScope::Repository {
            filter: RepoFilter::Slug("samzong/Recall".to_string()),
            local_root: None,
        },
        thread_role: None,
        excluded_session_id: None,
    };
    let results = engine.hybrid_search("parser", None, &filters, 10, 3).unwrap();
    let mut ids: Vec<String> = results.into_iter().map(|result| result.session.id).collect();
    ids.sort();

    assert_eq!(ids, vec!["s1".to_string(), "s2".to_string()]);
}

#[test]
fn repo_name_filter_fails_when_ambiguous() {
    let store = setup();
    let mut first = make_session("s1", "codex", "raw1", "First");
    first.repo_slug = Some("samzong/Recall".to_string());
    first.repo_name = Some("Recall".to_string());
    let mut second = make_session("s2", "opencode", "raw2", "Second");
    second.repo_slug = Some("other/Recall".to_string());
    second.repo_name = Some("Recall".to_string());
    store.insert_session(&first).unwrap();
    store.insert_session(&second).unwrap();

    let err = store.resolve_repo_filter("Recall").unwrap_err().to_string();
    assert!(err.contains("ambiguous"));
    assert!(err.contains("samzong/Recall"));
    assert!(err.contains("other/Recall"));
}

#[test]
fn project_filter_prefers_indexed_relative_directory() {
    let store = setup();
    let mut session = make_session("s1", "codex", "raw1", "Relative directory");
    session.directory = Some("samzong/Recall".to_string());
    store.insert_session(&session).unwrap();

    let scope = store.resolve_scope(Some("samzong/Recall"), None).unwrap().scope;

    assert_eq!(scope, ProjectScope::Directory("samzong/Recall".to_string()));
}

#[test]
fn repository_scope_reaches_local_checkout_without_repo_identity() {
    let store = setup();
    let mut indexed = make_session("s1", "codex", "raw1", "Backfilled");
    indexed.directory = Some("/repo/root".to_string());
    indexed.repo_remote = Some("github.com/samzong/Recall".to_string());
    let mut not_backfilled = make_session("s2", "codex", "raw2", "Missing identity");
    not_backfilled.directory = Some("/repo/root/nested".to_string());
    let mut other = make_session("s3", "codex", "raw3", "Other repo");
    other.directory = Some("/elsewhere".to_string());
    for session in [&indexed, &not_backfilled, &other] {
        store.insert_session(session).unwrap();
    }

    let scope = ProjectScope::Repository {
        filter: RepoFilter::Remote("github.com/samzong/Recall".to_string()),
        local_root: Some("/repo/root".to_string()),
    };
    let mut ids = store
        .list_recent_sessions_for_search_scope(None, TimeRange::All, &scope, None, 10)
        .unwrap()
        .into_iter()
        .map(|session| session.source_id)
        .collect::<Vec<_>>();
    ids.sort();

    assert_eq!(ids, vec!["raw1".to_string(), "raw2".to_string()]);
}

#[test]
fn scope_predicate_matches_sql_and_rust_paths() {
    let store = setup();
    let fixtures = [
        ("raw1", Some("/repo/root"), Some("github.com/samzong/Recall"), Some("samzong/Recall")),
        (
            "raw2",
            Some("/repo/root/nested"),
            Some("github.com/samzong/Recall"),
            Some("samzong/Recall"),
        ),
        ("raw3", Some("/repo/worktree"), Some("github.com/samzong/Recall"), Some("samzong/Recall")),
        ("raw4", Some("/repo/rootless"), None, None),
        ("raw5", Some("/elsewhere"), Some("github.com/other/Repo"), Some("other/Repo")),
        ("raw6", None, None, None),
        ("raw7", Some("/work/foo_bar/child"), None, None),
        ("raw8", Some("/work/fooXbar/child"), None, None),
        ("raw9", Some("/work/100%/child"), None, None),
        ("raw10", Some("/work/100X/child"), None, None),
        ("raw11", Some(r"C:\\repo"), None, None),
        ("raw12", Some(r"C:\\repo\\child"), None, None),
        ("raw13", Some(r"C:\\repository\\child"), None, None),
    ];
    for (index, (source_id, directory, remote, slug)) in fixtures.iter().enumerate() {
        let mut session = make_session(&format!("s{index}"), "codex", source_id, "Fixture");
        session.directory = directory.map(str::to_string);
        session.repo_remote = remote.map(str::to_string);
        session.repo_slug = slug.map(str::to_string);
        session.repo_name = slug.map(|slug| slug.rsplit('/').next().unwrap().to_string());
        store.insert_session(&session).unwrap();
    }

    let scopes = [
        ProjectScope::Global,
        ProjectScope::Directory("/repo/root".to_string()),
        ProjectScope::Directory("/repo/root/".to_string()),
        ProjectScope::Directory("/repo".to_string()),
        ProjectScope::Repository {
            filter: RepoFilter::Remote("github.com/samzong/Recall".to_string()),
            local_root: None,
        },
        ProjectScope::Repository {
            filter: RepoFilter::Remote("github.com/samzong/Recall".to_string()),
            local_root: Some("/repo/rootless".to_string()),
        },
        ProjectScope::Repository {
            filter: RepoFilter::Slug("samzong/Recall".to_string()),
            local_root: None,
        },
        ProjectScope::Directory("/work/foo_bar".to_string()),
        ProjectScope::Directory("/work/100%".to_string()),
        ProjectScope::Directory(r"C:\\repo".to_string()),
        ProjectScope::Directory(r"C:\\repo\\".to_string()),
    ];

    for scope in scopes {
        let mut sql_ids = store
            .list_recent_sessions_for_search_scope(None, TimeRange::All, &scope, None, 100)
            .unwrap()
            .into_iter()
            .map(|session| session.source_id)
            .collect::<Vec<_>>();
        sql_ids.sort();

        let mut rust_ids = fixtures
            .iter()
            .filter(|(_, directory, remote, slug)| {
                scope.matches(SessionScopeFields {
                    directory: *directory,
                    repo_remote: *remote,
                    repo_slug: *slug,
                    repo_name: slug.map(|slug| slug.rsplit('/').next().unwrap()),
                })
            })
            .map(|(source_id, ..)| source_id.to_string())
            .collect::<Vec<_>>();
        rust_ids.sort();

        assert_eq!(sql_ids, rust_ids, "scope {scope:?} disagrees between SQL and Rust");

        let expected: Option<&[&str]> = match &scope {
            ProjectScope::Directory(directory) if directory == "/work/foo_bar" => Some(&["raw7"]),
            ProjectScope::Directory(directory) if directory == "/work/100%" => Some(&["raw9"]),
            ProjectScope::Directory(directory) if directory == r"C:\\repo" => {
                Some(&["raw11", "raw12"])
            }
            _ => None,
        };
        if let Some(expected) = expected {
            assert_eq!(sql_ids, expected, "scope {scope:?} matched the wrong sessions");
        }
    }
}

#[test]
fn project_filter_all_selects_global_scope() {
    let store = setup();
    let mut session = make_session("s1", "codex", "raw1", "Indexed");
    session.repo_slug = Some("samzong/Recall".to_string());
    session.repo_name = Some("Recall".to_string());
    store.insert_session(&session).unwrap();

    assert_eq!(store.resolve_scope(Some("all"), None).unwrap().scope, ProjectScope::Global);
}

#[test]
fn project_filter_reports_unknown_name_instead_of_matching_nothing() {
    let store = setup();
    store.insert_session(&make_session("s1", "codex", "raw1", "Indexed")).unwrap();

    let err = store.resolve_scope(Some("Unindexed"), None).unwrap_err().to_string();

    assert!(err.contains("no indexed project matches"), "{err}");
}

#[test]
fn repo_name_filter_keeps_working_without_indexed_slug() {
    let store = setup();
    let mut session = make_session("s1", "codex", "raw1", "Imported");
    session.repo_name = Some("Recall".to_string());
    store.insert_session(&session).unwrap();

    assert_eq!(
        store.resolve_repo_filter("Recall").unwrap(),
        RepoFilter::Name("Recall".to_string())
    );
}

#[test]
fn hybrid_search_filters_by_thread_role_in_sql() {
    use crate::db::search::ThreadRoleFilter;
    use crate::db::store::SessionTopologyWrite;
    use crate::types::ThreadRole;

    let store = setup();
    let persist = |id: &str, source_id: &str, role: ThreadRole| {
        let session = make_session(id, "codex", source_id, "Topology search");
        let messages = vec![make_message(id, Role::User, "cloudflare deploy token", 0)];
        store
            .persist_session_with_usage_and_events_with_topology(
                &session,
                &messages,
                &[],
                None,
                &[],
                None,
                &SessionTopologyWrite {
                    thread_role: Some(role),
                    parents: &[],
                    parser_version: Some(1),
                },
            )
            .unwrap();
    };
    persist("p", "primary-src", ThreadRole::Primary);
    persist("s", "sub-src", ThreadRole::Subagent);

    let engine = SearchEngine::new(&store.conn);
    let filter = |thread_role| SearchFilters {
        sources: None,
        time_range: TimeRange::All,
        scope: ProjectScope::Global,
        thread_role,
        excluded_session_id: None,
    };
    let source_ids = |results: Vec<crate::types::SearchResult>| {
        let mut ids = results.into_iter().map(|r| r.session.source_id).collect::<Vec<_>>();
        ids.sort();
        ids
    };

    let all = engine.hybrid_search("cloudflare", None, &filter(None), 10, 3).unwrap();
    assert_eq!(source_ids(all), vec!["primary-src".to_string(), "sub-src".to_string()]);

    let subs = engine
        .hybrid_search("cloudflare", None, &filter(Some(ThreadRoleFilter::Subagent)), 10, 3)
        .unwrap();
    assert_eq!(source_ids(subs), vec!["sub-src".to_string()]);

    let prims = engine
        .hybrid_search("cloudflare", None, &filter(Some(ThreadRoleFilter::Primary)), 10, 3)
        .unwrap();
    assert_eq!(source_ids(prims), vec!["primary-src".to_string()]);
}
