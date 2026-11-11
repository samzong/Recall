use super::*;
use crate::db::search::{SearchEngine, SearchFilters, TimeRange};
use crate::db::store::Store;
use crate::project_scope::ProjectScope;

fn filters() -> SearchFilters {
    SearchFilters {
        sources: None,
        time_range: TimeRange::All,
        scope: ProjectScope::Global,
        thread_role: None,
        excluded_session_id: None,
    }
}

#[test]
fn v20_converges_missing_partial_and_complete_legacy_indexes() {
    register_sqlite_vec();
    for legacy in ["missing-column", "missing", "partial", "complete"] {
        let store = Store::open_in_memory().unwrap();
        store
            .conn
            .execute_batch(
                "INSERT INTO sessions(id, source, source_id, title, started_at)
             VALUES ('s', 'test', 's', 'migration', 1);
             INSERT INTO messages(session_id, role, content, seq, trigram_indexed)
             VALUES ('s', 'user', '历史消息包含数据库迁移', 0, NULL),
                    ('s', 'user', '另一条包含数据库迁移', 1, 1),
                    ('s', 'user', 'ordinary English', 2, 0);
             PRAGMA user_version=19;",
            )
            .unwrap();
        if legacy.starts_with("missing") {
            store
                .conn
                .execute_batch(
                    "DROP TRIGGER messages_trigram_ai; DROP TRIGGER messages_trigram_ad;
                 DROP TABLE messages_fts_trigram;",
                )
                .unwrap();
            if legacy == "missing-column" {
                store
                    .conn
                    .execute_batch("ALTER TABLE messages DROP COLUMN trigram_indexed;")
                    .unwrap();
            }
        } else if legacy == "partial" {
            store
                .conn
                .execute_batch(
                    "INSERT INTO messages_fts_trigram(messages_fts_trigram, rowid, content)
                 SELECT 'delete', id, content FROM messages WHERE seq=1;",
                )
                .unwrap();
        }
        init(&store.conn).unwrap();
        assert_eq!(schema_version(&store.conn).unwrap(), 20);
        let engine = SearchEngine::new(&store.conn);
        assert!(engine.search_messages("数据库迁移", &filters(), None, 50).is_err());
        assert!(engine.hybrid_search("数据库迁移", None, &filters(), 20, 3).is_err());
        assert_eq!(engine.search_messages("ordinary", &filters(), None, 50).unwrap().len(), 1);
        while store.backfill_trigram_batch().unwrap() {}
        store.finish_trigram_backfill().unwrap();
        assert_eq!(engine.search_messages("数据库迁移", &filters(), None, 50).unwrap().len(), 2);
        init(&store.conn).unwrap();
        assert!(!store.backfill_trigram_batch().unwrap());
        store.conn.execute_batch("DELETE FROM messages;").unwrap();
        assert!(engine.search_messages("数据库迁移", &filters(), None, 50).unwrap().is_empty());
        store.conn.execute_batch(
            "INSERT INTO messages_fts_trigram(messages_fts_trigram) VALUES ('integrity-check');",
        ).unwrap();
    }
}

#[test]
fn v20_resumes_after_reopen_and_keeps_concurrent_and_legacy_writes_searchable() {
    register_sqlite_vec();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recall.db");
    let store = Store::open_at(&path).unwrap();
    store
        .conn
        .execute_batch(
            "INSERT INTO sessions(id, source, source_id, title, started_at)
         VALUES ('s', 'test', 's', 'migration', 1);
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<260)
         INSERT INTO messages(session_id, role, content, seq, trigram_indexed)
         SELECT 's', 'user', '历史消息包含数据库迁移', i, 1 FROM n;
         PRAGMA user_version=19;",
        )
        .unwrap();
    init(&store.conn).unwrap();
    assert!(store.backfill_trigram_batch().unwrap());
    drop(store);
    let store = Store::open_at(&path).unwrap();
    let writer = Store::open_at(&path).unwrap();
    writer
        .conn
        .execute_batch(
            "DELETE FROM messages WHERE id=(
             SELECT json_extract(detail, '$.cursor') FROM background_job_state WHERE job='trigram'
         );
         DELETE FROM messages WHERE seq IN (1, 200);
         INSERT INTO messages(session_id, role, content, seq)
         VALUES ('s', 'user', '降级写入数据库迁移', 300);
         INSERT INTO messages(session_id, role, content, seq, trigram_indexed)
         VALUES ('s', 'user', '新增消息数据库迁移', 301, 1);
         DELETE FROM messages WHERE seq=301;",
        )
        .unwrap();
    while store.backfill_trigram_batch().unwrap() {}
    store.finish_trigram_backfill().unwrap();
    let count: i64 = store.conn.query_row(
        "SELECT count(*) FROM messages_fts_trigram WHERE messages_fts_trigram MATCH '数据库迁移'",
        [], |r| r.get(0),
    ).unwrap();
    assert_eq!(count, 258);
    writer.conn.execute_batch("DELETE FROM messages;").unwrap();
    store
        .conn
        .execute_batch(
            "INSERT INTO messages_fts_trigram(messages_fts_trigram) VALUES ('integrity-check');",
        )
        .unwrap();
}

#[test]
fn v20_missing_or_incomplete_job_state_cannot_corrupt_unindexed_deletions() {
    register_sqlite_vec();
    for broken_state in [
        "DELETE FROM background_job_state WHERE job='trigram';",
        "UPDATE background_job_state SET detail='{}' WHERE job='trigram';",
    ] {
        let store = Store::open_in_memory().unwrap();
        store.conn.execute_batch(
            "INSERT INTO sessions(id,source,source_id,title,started_at) VALUES ('s','test','s','state',1);
             INSERT INTO messages(session_id,role,content,seq,trigram_indexed)
             VALUES ('s','user','数据库迁移',1,1),('s','user','数据库迁移',2,1);
             PRAGMA user_version=19;",
        ).unwrap();
        init(&store.conn).unwrap();
        let detail: String = store
            .conn
            .query_row("SELECT detail FROM background_job_state WHERE job='trigram'", [], |r| {
                r.get(0)
            })
            .unwrap();
        store.conn.execute_batch(broken_state).unwrap();
        assert!(store.backfill_trigram_batch().is_err());
        store.conn.execute_batch("DELETE FROM messages WHERE seq=2;").unwrap();
        assert!(
            SearchEngine::new(&store.conn)
                .search_messages("数据库迁移", &filters(), None, 50)
                .is_err()
        );
        store.conn.execute(
            "INSERT INTO background_job_state(job,phase,detail,updated_at) VALUES ('trigram','backfill',?1,0)
             ON CONFLICT(job) DO UPDATE SET detail=excluded.detail", [detail],
        ).unwrap();
        while store.backfill_trigram_batch().unwrap() {}
        store.finish_trigram_backfill().unwrap();
        let averages: Vec<u8> = store
            .conn
            .query_row("SELECT block FROM messages_fts_trigram_data WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(averages[0], 1);
        assert_eq!(
            SearchEngine::new(&store.conn)
                .search_messages("数据库迁移", &filters(), None, 50)
                .unwrap()
                .len(),
            1
        );
    }
}
