use super::*;

#[test]
fn missing_index_is_a_tool_error() {
    let index = IndexState::Unavailable { path: None, message: MISSING_INDEX.to_string() };
    let args = SearchSessionsArgs { query: "error".into(), ..Default::default() };
    let list = search_sessions(&index, &args).expect_err("missing index");
    assert!(list.hits.is_empty());
    assert_eq!(list.message.as_deref(), Some(MISSING_INDEX));
    assert_eq!(json_result(search_sessions(&index, &args)).is_error, Some(true));
}

#[test]
fn empty_index_search_and_list_explain_the_gap() {
    let store = setup();
    let index = ready(store);
    let search_args = SearchSessionsArgs { query: "anything".into(), ..Default::default() };
    let search = search_sessions(&index, &search_args).unwrap();
    assert!(search.hits.is_empty());
    assert_eq!(search.message.as_deref(), Some(EMPTY_INDEX));
    assert_eq!(json_result(search_sessions(&index, &search_args)).is_error, Some(false));

    let list_args = ListRecentSessionsArgs { ..Default::default() };
    let listed = list_recent_sessions(&index, &list_args).unwrap();
    assert!(listed.hits.is_empty());
    assert_eq!(listed.message.as_deref(), Some(EMPTY_INDEX));
    assert_eq!(json_result(list_recent_sessions(&index, &list_args)).is_error, Some(false));

    let history = file_history(&index, &file_history_args("src/db/schema.rs")).unwrap();
    assert!(history.events.is_empty());
    assert_eq!(history.message.as_deref(), Some(EMPTY_INDEX));
    assert_eq!(
        json_result(file_history(&index, &file_history_args("src/db/schema.rs"))).is_error,
        Some(false)
    );
}

#[test]
fn search_reuses_hybrid_search_and_clamps_limit() {
    let store = setup();
    store.insert_session(&session("s1", "codex", "iterator panic", 2_000)).unwrap();
    store
        .insert_messages(&[message("s1", Role::User, "how do I use iterators in Rust", 0)])
        .unwrap();
    let index = ready(store);

    let list = search_sessions(
        &index,
        &SearchSessionsArgs { query: "iterators".into(), limit: Some(80), ..Default::default() },
    )
    .unwrap();
    assert_eq!(list.hits.len(), 1);
    assert_eq!(list.hits[0].session_id, "s1");
    assert_eq!(list.hits[0].source_session_id, "src-s1");
    assert_eq!(list.hits[0].source, "codex");
    assert_eq!(list.hits[0].project.as_deref(), Some("/tmp/demo"));
    assert_eq!(list.hits[0].title, "iterator panic");
    assert!(list.hits[0].excerpt.as_deref().unwrap().contains("iterators"));
    assert_eq!(list.hits[0].timestamp, iso8601(2_000));
    assert!(list.message.is_none());
    assert_eq!(
        without_fields(&list.hits[0], &["source_session_id"]),
        serde_json::json!({
            "session_id": "s1",
            "source": "codex",
            "project": "/tmp/demo",
            "title": "iterator panic",
            "excerpt": "how do I use iterators in Rust",
            "timestamp": iso8601(2_000),
        })
    );
}

#[test]
fn list_recent_is_newest_first_and_matches_search_shape() {
    let store = setup();
    store.insert_session(&session("old", "codex", "older", 1_000)).unwrap();
    store.insert_session(&session("new", "claude-code", "newer", 9_000)).unwrap();
    let index = ready(store);

    let list = list_recent_sessions(
        &index,
        &ListRecentSessionsArgs { limit: Some(10), ..Default::default() },
    )
    .unwrap();
    assert_eq!(list.hits.len(), 2);
    assert_eq!(list.hits[0].session_id, "new");
    assert_eq!(list.hits[0].source_session_id, "src-new");
    assert_eq!(list.hits[0].excerpt.as_deref(), Some("newer summary"));
    assert_eq!(list.hits[1].session_id, "old");
    assert_eq!(list.hits[1].source_session_id, "src-old");
}

#[test]
fn host_identity_requires_verified_source_native_values() {
    let codex_id = "019c9c4f-a462-7cc1-99a5-4ab521648c91";
    let codex = CurrentSessionContext::from_values(None, Some(codex_id), Some(codex_id));
    assert_eq!(
        codex.host_identity,
        Some(SourceSessionIdentity {
            source: "codex".to_string(),
            source_session_id: codex_id.to_string(),
        })
    );
    assert!(
        CurrentSessionContext::from_values(None, Some("thread"), Some("session"))
            .host_identity
            .is_none()
    );
    assert!(CurrentSessionContext::from_values(None, Some(codex_id), None).host_identity.is_none());
    assert!(CurrentSessionContext::from_values(None, Some(" "), Some(" ")).host_identity.is_none());
    assert!(
        CurrentSessionContext::from_values(None, Some("same"), Some("same"))
            .host_identity
            .is_none()
    );
    let claude_id = "604c4e71-f49c-4cc0-9388-88905fe65473";
    let claude = CurrentSessionContext::from_values(Some(claude_id), None, None);
    assert_eq!(
        claude.host_identity,
        Some(SourceSessionIdentity {
            source: "claude-code".to_string(),
            source_session_id: claude_id.to_string(),
        })
    );
    assert!(
        CurrentSessionContext::from_values(Some(claude_id), Some(codex_id), Some(codex_id))
            .host_identity
            .is_none()
    );
}

#[test]
fn resolved_current_session_is_excluded_before_search_and_recent_limits() {
    let store = setup();
    let mut current = session("000-current", "codex", "current", 100_000);
    current.message_count = 1;
    store.insert_session(&current).unwrap();
    store.insert_messages(&[message("000-current", Role::User, "identityneedle", 0)]).unwrap();
    for index in 0..51 {
        let id = format!("history-{index:02}");
        let mut stored = session(&id, "codex", "history", 50_000 - index);
        stored.message_count = 1;
        store.insert_session(&stored).unwrap();
        store.insert_messages(&[message(&id, Role::User, "identityneedle", 0)]).unwrap();
    }
    store
        .persist_topology_for_existing_session(
            "codex",
            "src-history-00",
            &SessionTopologyWrite {
                thread_role: Some(ThreadRole::Primary),
                parents: &[],
                parser_version: Some(1),
            },
        )
        .unwrap();
    let spawn = ParentLink {
        relation: ParentRelation::Spawn,
        source: "codex".to_string(),
        source_id: "src-history-00".to_string(),
    };
    store
        .persist_topology_for_existing_session(
            "codex",
            "src-history-01",
            &SessionTopologyWrite {
                thread_role: Some(ThreadRole::Subagent),
                parents: std::slice::from_ref(&spawn),
                parser_version: Some(1),
            },
        )
        .unwrap();
    let index = ready(store);
    let current_context = context("codex", "src-000-current");

    let first = search_sessions_with_context(
        &index,
        &SearchSessionsArgs {
            query: "identityneedle".into(),
            limit: Some(1),
            ..Default::default()
        },
        &current_context,
    )
    .unwrap();
    assert_eq!(first.hits.len(), 1);
    assert_ne!(first.hits[0].session_id, "000-current");
    assert_eq!(first.current_session.resolution, CurrentSessionResolution::Resolved);
    assert_eq!(first.current_session.session_id.as_deref(), Some("000-current"));

    let fifty = search_sessions_with_context(
        &index,
        &SearchSessionsArgs {
            query: "identityneedle".into(),
            limit: Some(50),
            ..Default::default()
        },
        &current_context,
    )
    .unwrap();
    assert_eq!(fifty.hits.len(), 50);
    assert!(fifty.hits.iter().all(|hit| hit.session_id != "000-current"));

    let recent = list_recent_sessions_with_context(
        &index,
        &ListRecentSessionsArgs { limit: Some(1), ..Default::default() },
        &current_context,
    )
    .unwrap();
    assert_eq!(recent.hits.len(), 1);
    assert_ne!(recent.hits[0].session_id, "000-current");
    assert_eq!(recent.current_session.resolution, CurrentSessionResolution::Resolved);

    let recent_fifty = list_recent_sessions_with_context(
        &index,
        &ListRecentSessionsArgs { limit: Some(50), ..Default::default() },
        &current_context,
    )
    .unwrap();
    assert_eq!(recent_fifty.hits.len(), 50);
    assert!(recent_fifty.hits.iter().all(|hit| hit.session_id != "000-current"));
}

#[test]
fn unresolved_or_mismatched_host_identity_keeps_discovery_complete() {
    let store = setup();
    let mut stored = session("current", "claude-code", "current", 10_000);
    stored.source_id = "shared-source-id".to_string();
    store.insert_session(&stored).unwrap();
    store.insert_messages(&[message("current", Role::User, "mismatchneedle", 0)]).unwrap();
    let index = ready(store);

    for current_context in [
        context("codex", "shared-source-id"),
        context("codex", "not-indexed"),
        CurrentSessionContext::from_values(None, Some("a"), Some("b")),
    ] {
        let result = search_sessions_with_context(
            &index,
            &SearchSessionsArgs {
                query: "mismatchneedle".into(),
                limit: Some(1),
                ..Default::default()
            },
            &current_context,
        )
        .unwrap();
        assert_eq!(result.current_session, CurrentSession::unknown());
        assert_eq!(result.hits[0].session_id, "current");
    }
}

#[test]
fn discovery_scope_does_not_change_exact_current_session_resolution() {
    let store = setup();
    let current = session("current", "codex", "current", 10_000);
    store.insert_session(&current).unwrap();
    let mut other = session("other", "claude-code", "other", 9_000);
    other.directory = Some("/tmp/other".to_string());
    store.insert_session(&other).unwrap();
    store.insert_messages(&[message("other", Role::User, "scopeneedle", 0)]).unwrap();
    let result = search_sessions_with_context(
        &ready(store),
        &SearchSessionsArgs {
            query: "scopeneedle".into(),
            project: Some("/tmp/other".into()),
            source: Some("claude-code".into()),
            limit: Some(1),
            invocation_nonce: None,
        },
        &context("codex", "src-current"),
    )
    .unwrap();
    assert_eq!(result.current_session.resolution, CurrentSessionResolution::Resolved);
    assert_eq!(result.hits[0].session_id, "other");
}

#[test]
fn exact_reads_remain_complete_for_the_resolved_current_session() {
    let store = setup();
    let mut current = session("current", "codex", "current", 10_000);
    current.message_count = 1;
    store.insert_session(&current).unwrap();
    store.insert_messages(&[message("current", Role::User, "exact transcript", 0)]).unwrap();
    persist_events(
        &store,
        "codex",
        "current",
        &[event(0, "file_write", Some("src/current.rs"), Some(10_000), None, None)],
    );
    let index = ready(store);

    let detail =
        get_session(&index, &GetSessionArgs { session_id: "current".into(), ..Default::default() })
            .unwrap();
    assert_eq!(detail.messages, "[user] exact transcript");
    let history = file_history(&index, &file_history_args("src/current.rs")).unwrap();
    assert_eq!(history.events.len(), 1);
    assert_eq!(history.events[0].session_id, "current");
}

#[test]
fn invocation_nonce_resolves_only_one_complete_indexed_candidate() {
    let store = setup();
    let mut current = session("current", "codex", "current", 10_000);
    current.source_id = "nonce-source".to_string();
    store.insert_session(&current).unwrap();
    let context = CurrentSessionContext::default();

    let resolved = context.resolve_with_probe(&store, Some("nonce"), |_| {
        probe_result(&[("codex", "nonce-source")], true)
    });
    assert_eq!(resolved.resolution, CurrentSessionResolution::Resolved);
    assert_eq!(resolved.session_id.as_deref(), Some("current"));

    for result in [
        probe_result(&[], true),
        probe_result(&[("codex", "nonce-source"), ("claude-code", "other")], true),
        probe_result(&[("codex", "nonce-source")], false),
        probe_result(&[("codex", "unindexed")], true),
        probe_result(&[("claude-code", "nonce-source")], true),
    ] {
        assert_eq!(
            context.resolve_with_probe(&store, Some("nonce"), |_| result),
            CurrentSession::unknown()
        );
    }
    assert_eq!(
        context.resolve_with_probe(&store, Some(" "), |_| { panic!("blank nonce must not probe") }),
        CurrentSession::unknown()
    );
}

#[test]
fn message_search_opens_sparse_context_and_preserves_explicit_current_session_reads() {
    let store = setup();
    store.insert_session(&session("s1", "codex", "Evidence", 2000)).unwrap();
    store
        .insert_messages(&[
            message("s1", Role::User, "What fixed it?", 0),
            message("s1", Role::Assistant, "evidencekeyword fixed it", 83),
            message("s1", Role::User, "Confirmed", 100),
        ])
        .unwrap();
    let index = ready(store);
    let args: SearchMessagesArgs =
        serde_json::from_value(serde_json::json!({"query": "evidencekeyword"})).unwrap();
    let ctx = context("codex", "src-s1");
    let hidden = search_message_matches(&index, &args, &ctx).unwrap();
    assert!(hidden["matches"].as_array().unwrap().is_empty());
    let args = SearchMessagesArgs { session_id: Some("s1".into()), ..args };
    let hits = search_message_matches(&index, &args, &ctx).unwrap();
    assert_eq!(hits["matches"][0]["seq"], 83);
    assert_eq!(hits["current_session_excluded"], false);
    let detail = get_session(
        &index,
        &GetSessionArgs { session_id: "s1".into(), around_seq: Some(83), ..Default::default() },
    )
    .unwrap();
    assert_eq!((detail.first_message_seq, detail.last_message_seq), (Some(0), Some(100)));
    assert_eq!(detail.returned_messages, 3);
    assert!(detail.messages.contains("[83][assistant] evidencekeyword"));
    assert!(!detail.truncated);
    assert!(
        get_session(
            &index,
            &GetSessionArgs { session_id: "s1".into(), around_seq: Some(82), ..Default::default() }
        )
        .is_err()
    );
    assert!(
        get_session(
            &index,
            &GetSessionArgs {
                session_id: "s1".into(),
                around_seq: Some(83),
                tail: true,
                ..Default::default()
            }
        )
        .is_err()
    );
}

#[test]
fn discovery_and_message_search_sanitize_terminal_noise_and_water_fill_inline_budget() {
    let store = setup();
    let short_clean = "简短命中保留全文 keywordneedle";
    for idx in 0..10 {
        let id = format!("s{idx:02}");
        let mut stored = session(&id, "codex", &"标题汉字".repeat(50), 10_000 - i64::from(idx));
        stored.summary = Some(if idx == 0 {
            "\x1b[32m进度 10%\r进度 100%\x1b[0m\r\n\x1b]8;;https://example.com\x07短摘要\x1b]8;;\x07"
                .to_string()
        } else {
            format!("\x1b[36m旧帧\r新帧-{idx}\x1b[0m\r\n{}", "摘".repeat(220))
        });
        store.insert_session(&stored).unwrap();
        let body = if idx == 0 {
            format!(
                "\x1b[31m10%\r100%\x1b[0m\r\n\x1b]8;;https://x.test\x07{short_clean}\x1b]8;;\x07"
            )
        } else {
            format!(
                "\x1b[35mframeA\rframeB-{idx}\x1b[0m\r\n{} keywordneedle {}",
                "前".repeat(220),
                "后".repeat(220)
            )
        };
        store.insert_messages(&[message(&id, Role::Assistant, &body, idx)]).unwrap();
    }
    let history_events: Vec<_> = (0..20)
        .map(|seq| {
            event(
                seq,
                "file_write",
                Some("src/mcp.rs"),
                Some(20_000 + i64::from(seq)),
                Some(&format!("\x1b[32m0%\r100%-{seq}\x1b[0m\r\n{}", "改".repeat(210))),
                Some("Edit"),
            )
        })
        .collect();
    persist_events(&store, "codex", "s01", &history_events);
    let index = ready(store);

    let msg_hits = search_message_matches(
        &index,
        &SearchMessagesArgs {
            search: SearchSessionsArgs { query: "keywordneedle".into(), ..Default::default() },
            session_id: None,
        },
        &CurrentSessionContext::default(),
    )
    .unwrap();
    let msg_bytes = serde_json::to_vec(&msg_hits).unwrap();
    assert!(msg_bytes.len() <= MCP_INLINE_MAX_BYTES, "search_messages {} bytes", msg_bytes.len());
    let matches = msg_hits["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 10);
    let mut keys: Vec<_> = matches[0].as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "alternative_versions",
            "excerpt",
            "locations",
            "role",
            "seq",
            "session_id",
            "source",
            "source_session_id",
            "timestamp",
            "title",
        ]
    );
    let short_hit = matches.iter().find(|hit| hit["session_id"] == "s00").unwrap();
    let short_excerpt = short_hit["excerpt"].as_str().unwrap();
    assert_eq!(short_excerpt, format!("100%\n{short_clean}"));
    assert!(!short_excerpt.ends_with('…'));
    for hit in matches.iter().filter(|hit| hit["session_id"] != "s00") {
        let excerpt = hit["excerpt"].as_str().unwrap();
        assert!(!excerpt.contains('\x1b'));
        assert!(!excerpt.contains('\r'));
        assert!(!excerpt.contains("frameA"));
        assert!(excerpt.ends_with('…'));
    }

    let sess_hits = search_sessions(
        &index,
        &SearchSessionsArgs { query: "keywordneedle".into(), ..Default::default() },
    )
    .unwrap();
    assert!(serde_json::to_vec(&sess_hits).unwrap().len() <= MCP_INLINE_MAX_BYTES);
    assert!(sess_hits.hits.iter().all(|h| {
        let ex = h.excerpt.as_deref().unwrap_or_default();
        !ex.contains('\x1b') && !ex.contains('\r')
    }));

    let recent_hits = list_recent_sessions(&index, &ListRecentSessionsArgs::default()).unwrap();
    assert!(serde_json::to_vec(&recent_hits).unwrap().len() <= MCP_INLINE_MAX_BYTES);
    assert_eq!(recent_hits.hits[0].excerpt.as_deref(), Some("进度 100%\n短摘要"));

    let legacy_history = file_history(&index, &file_history_args("src/mcp.rs")).unwrap();
    assert_eq!(legacy_history.events.len(), 20);
    assert!(serde_json::to_vec(&legacy_history).unwrap().len() <= MCP_INLINE_MAX_BYTES);
    assert!(legacy_history.events[0].summary.as_deref().unwrap().starts_with("100%-19\n"));
}
