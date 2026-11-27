use super::*;

#[test]
fn event_evidence_pages_native_payload_and_binds_related_revisions() {
    let store = setup();
    for id in ["s1", "s2"] {
        store.insert_session(&session(id, "cursor", "Evidence", 1000)).unwrap();
    }
    store
        .insert_messages(&[message("s1", Role::User, "Explain the requested change", 10)])
        .unwrap();
    let mut call = event(3, "tool_call", Some("file"), None, Some("summary"), Some("apply_patch"));
    call.message_seq = Some(10);
    call.tool_call_id = Some("shared-call-id".into());
    call.source_event_id = Some("call-record".into());
    call.source_path = Some("composer:src-s1".into());
    call.attrs_json = Some(serde_json::json!({"input":"€\n\\\"".repeat(900)}).to_string());
    let mut result = event(4, "tool_result", None, None, Some("result"), None);
    result.tool_call_id = call.tool_call_id.clone();
    result.source_event_id = Some("r".repeat(3500));
    persist_events(&store, "cursor", "s1", &[call.clone(), result.clone()]);
    persist_events(&store, "cursor", "s2", &[result.clone()]);
    let reference = {
        let tx = store.conn.unchecked_transaction().unwrap();
        let id = tx
            .query_row(
                "SELECT id FROM session_events WHERE session_id='s1' AND event_seq=3",
                [],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::to_string(&crate::db::event_store::event_reference(&tx, id).unwrap()).unwrap()
    };
    let index = ready(store);
    let mut args = GetSessionArgs {
        session_id: "s1".into(),
        event_ref: Some(reference.clone()),
        max_bytes: Some(2048),
        ..Default::default()
    };
    args.evidence_part = Some(crate::event_evidence::EvidencePart::Before);
    let missing = get_event_evidence(&index, &args).unwrap_err();
    assert!(missing.starts_with("content_reference_not_recorded"));
    assert!(missing.contains("payload.related_event_refs"));
    args.evidence_part = None;
    let mut body = String::new();
    let mut first_cursor = None;
    for _ in 0..200 {
        let page = get_event_evidence(&index, &args).unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= 2048);
        let page = serde_json::to_value(page).unwrap();
        assert_eq!(page["byte_offset"].as_u64().unwrap() as usize, body.len());
        body.push_str(page["data"].as_str().unwrap());
        args.cursor = page["next_cursor"].as_str().map(str::to_string);
        if first_cursor.is_none() {
            first_cursor = args.cursor.clone();
        }
        if args.cursor.is_none() {
            break;
        }
    }
    assert!(args.cursor.is_none());
    let document: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(document["event"]["attrs_json"], call.attrs_json.as_deref().unwrap());
    assert_eq!(document["related_event_refs"].as_array().unwrap().len(), 1);
    let related = GetSessionArgs {
        session_id: "s1".into(),
        event_ref: document["related_event_refs"][0].as_str().map(str::to_string),
        ..Default::default()
    };
    assert!(get_event_evidence(&index, &related).is_ok());
    let discussion: GetSessionArgs =
        serde_json::from_value(document["discussion"].clone()).unwrap();
    assert!(get_session(&index, &discussion).unwrap().messages.contains("requested change"));
    let IndexState::Ready(store) = &index else { panic!() };
    persist_events(store, "cursor", "s1", &[call.clone(), result.clone()]);
    assert!(get_event_evidence(&index, &args).unwrap_err().contains("stale"));
    result.attrs_json = Some("changed result".into());
    persist_events(store, "cursor", "s1", &[call.clone(), result.clone()]);
    args.cursor = first_cursor;
    assert!(get_event_evidence(&index, &args).unwrap_err().contains("stale"));
    args.cursor = None;
    let tx = store.conn.unchecked_transaction().unwrap();
    let id = tx
        .query_row(
            "SELECT id FROM session_events WHERE session_id = 's1' AND event_seq = 3",
            [],
            |row| row.get(0),
        )
        .unwrap();
    args.event_ref = Some(
        serde_json::to_string(&crate::db::event_store::event_reference(&tx, id).unwrap()).unwrap(),
    );
    drop(tx);
    assert!(get_event_evidence(&index, &args).is_ok());
    call.attrs_json = Some("changed call".into());
    persist_events(store, "cursor", "s1", &[call, result]);
    assert!(get_event_evidence(&index, &args).unwrap_err().contains("stale"));
}

#[test]
fn event_evidence_never_reads_imported_locators_and_rejects_oversized_reads() {
    let store = setup();
    let mut imported = session("s1", "cursor", "Imported evidence", 1000);
    imported.is_import = true;
    store.insert_session(&imported).unwrap();
    let mut record = event(0, "tool_result", None, None, None, None);
    record.source_path = Some("/private/untrusted/source.sqlite".into());
    record.attrs_json = Some("{\"afterContentId\":\"composer.content.opaque\"}".into());
    persist_events(&store, "cursor", "s1", &[record]);
    let reference = {
        let tx = store.conn.unchecked_transaction().unwrap();
        let id = tx.query_row("SELECT id FROM session_events", [], |row| row.get(0)).unwrap();
        serde_json::to_string(&crate::db::event_store::event_reference(&tx, id).unwrap()).unwrap()
    };
    let index = ready(store);
    let mut args = GetSessionArgs {
        session_id: "s1".into(),
        event_ref: Some(reference),
        evidence_part: Some(crate::event_evidence::EvidencePart::After),
        ..Default::default()
    };
    assert_eq!(get_event_evidence(&index, &args).unwrap_err(), "source_unverified");
    args.evidence_part = None;
    assert!(get_event_evidence(&index, &args).is_ok());
    let IndexState::Ready(store) = &index else { panic!() };
    store
        .conn
        .execute("UPDATE session_events SET attrs_json = CAST(zeroblob(67108865) AS TEXT)", [])
        .unwrap();
    assert_eq!(get_event_evidence(&index, &args).unwrap_err(), "evidence_budget_exceeded");
}

#[test]
fn event_evidence_defaults_to_inline_budget_and_packs_utf8_without_6x_waste() {
    let store = setup();
    store.insert_session(&session("s1", "cursor", "Evidence", 1_000)).unwrap();
    let mut call = event(1, "tool_call", Some("src/mcp.rs"), None, Some("edit"), Some("Edit"));
    call.attrs_json = Some(serde_json::json!({"patch": "€★☃─€★☃─".repeat(650)}).to_string());
    persist_events(&store, "cursor", "s1", std::slice::from_ref(&call));
    let reference = {
        let tx = store.conn.unchecked_transaction().unwrap();
        let id: i64 = tx
            .query_row("SELECT id FROM session_events WHERE session_id='s1'", [], |r| r.get(0))
            .unwrap();
        serde_json::to_string(&crate::db::event_store::event_reference(&tx, id).unwrap()).unwrap()
    };
    let index = ready(store);
    let mut args = GetSessionArgs {
        session_id: "s1".into(),
        event_ref: Some(reference),
        ..Default::default()
    };
    let mut full = String::new();
    let mut pages = 0;
    loop {
        let page = get_event_evidence(&index, &args).unwrap();
        let bytes = serde_json::to_vec(&page).unwrap();
        assert!(bytes.len() <= MCP_INLINE_MAX_BYTES, "evidence page {} bytes", bytes.len());
        if pages == 0 {
            assert!(bytes.len() > 7_500, "first page wasted inline budget: {} bytes", bytes.len());
        }
        let value = serde_json::to_value(&page).unwrap();
        assert_eq!(value["byte_offset"].as_u64().unwrap() as usize, full.len());
        full.push_str(value["data"].as_str().unwrap());
        pages += 1;
        args.cursor = value["next_cursor"].as_str().map(str::to_string);
        if args.cursor.is_none() {
            break;
        }
    }
    assert_eq!(pages, 2);
    let parsed: Value = serde_json::from_str(&full).unwrap();
    assert_eq!(parsed["event"]["attrs_json"], call.attrs_json.as_deref().unwrap());
}
