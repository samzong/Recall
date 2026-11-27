use super::*;

#[cfg(test)]
mod search_scope_stats_tests {
    use super::*;
    use crate::db::schema;
    use crate::types::Session;

    fn store() -> Store {
        schema::register_sqlite_vec();
        Store::open_in_memory().unwrap()
    }

    fn session(
        id: &str,
        source: &str,
        started_at: i64,
        message_count: u32,
        directory: Option<&str>,
    ) -> Session {
        Session {
            source: source.to_string(),
            source_id: format!("raw-{id}"),
            title: id.to_string(),
            directory: directory.map(str::to_string),
            started_at,
            updated_at: Some(started_at),
            message_count,
            ..crate::types::test_support::session(id)
        }
    }

    #[test]
    fn stats_for_search_scope_sum_session_counts_without_reading_messages() {
        let store = store();
        store.insert_session(&session("a", "codex", 10, 2, Some("/repo/a"))).unwrap();
        store.insert_session(&session("b", "codex", 20, 3, Some("/repo/b"))).unwrap();
        store.insert_session(&session("c", "opencode", 30, 4, Some("/other"))).unwrap();

        assert_eq!(
            store.stats_for_search_scope(None, TimeRange::All, &ProjectScope::Global).unwrap(),
            (3, 9)
        );
        assert_eq!(
            store
                .stats_for_search_scope(
                    Some(&["codex".to_string()]),
                    TimeRange::All,
                    &ProjectScope::Global
                )
                .unwrap(),
            (2, 5)
        );
        assert_eq!(
            store
                .stats_for_search_scope(
                    None,
                    TimeRange::All,
                    &ProjectScope::Directory("/repo/a".into())
                )
                .unwrap(),
            (1, 2)
        );
    }
}

#[cfg(test)]
mod source_stats_tests {
    use super::*;
    use crate::db::schema;

    #[test]
    fn indexed_source_stats_use_persisted_session_counts() {
        schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        store
            .conn
            .execute_batch(
                "INSERT INTO sessions
                    (id, source, source_id, title, started_at, message_count)
                 VALUES
                    ('c1', 'codex', 'raw-c1', 'one', 20, 2),
                    ('c2', 'codex', 'raw-c2', 'two', 10, 3),
                    ('o1', 'opencode', 'raw-o1', 'three', 30, 4);",
            )
            .unwrap();

        let stats = store.indexed_source_stats().unwrap();

        let codex = &stats["codex"];
        assert_eq!(codex.sessions, 2);
        assert_eq!(codex.messages, 5);
        assert_eq!(codex.oldest_started_at, Some(10));
        assert_eq!(codex.newest_started_at, Some(20));

        let opencode = &stats["opencode"];
        assert_eq!(opencode.sessions, 1);
        assert_eq!(opencode.messages, 4);
    }
}
