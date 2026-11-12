use super::*;

#[test]
fn export_usage_marks_global_ownership_without_removing_raw_events() {
    let store = setup();
    for (id, source, timestamp, output, reasoning, version) in [
        ("codex-a-owner", "codex", 10, 10, 4, 4),
        ("codex-b-copy", "codex", 10, 6, 4, 5),
        ("claude-owner", "claude-code", 1, 3, 0, 1),
        ("claude-copy", "claude-code", 20, 3, 0, 1),
        ("warp-owner", "warp", 1, 3, 0, 1),
        ("warp-copy", "warp", 20, 3, 0, 1),
        ("qoder-owner", "qoder", 1, 3, 0, 1),
        ("qoder-copy", "qoder", 20, 3, 0, 1),
        ("pi-first", "pi", 1, 3, 0, 2),
        ("pi-second", "pi", 20, 3, 0, 2),
    ] {
        let mut event = crate::types::test_support::usage_event("assistant:shared");
        event.timestamp = timestamp;
        event.output_tokens = output;
        event.reasoning_tokens = reasoning;
        event.parser_version = version;
        let mut events = vec![event.clone()];
        if id == "codex-a-owner" {
            event.event_key = "second-raw-row".to_string();
            event.event_seq = 1;
            events.push(event);
        }
        store
            .persist_session_with_usage(
                &make_session(id, source, id, id),
                &[],
                &events,
                Some(version),
            )
            .unwrap();
    }
    let mut options = ExportOptions {
        session_ids: Vec::new(),
        sources: None,
        time_range: TimeRange::All,
        scope: ProjectScope::Global,
        thread_role: None,
        limit: None,
        includes: ExportIncludes {
            messages: false,
            usage: true,
            events: false,
            usage_ownership: false,
        },
    };
    let export = |options: &ExportOptions| {
        let mut output = Vec::new();
        write_jsonl(&store, options, &mut output).unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>()
    };
    assert!(!crate::export::parse_export_includes(None).unwrap().usage_ownership);
    assert!(!crate::export::parse_export_includes(Some("all")).unwrap().usage_ownership);
    assert!(export(&options).iter().all(|record| {
        record["usage_events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event.get("is_counted").is_none())
    }));
    options.includes =
        crate::export::apply_include_filter(options.includes, "usage-ownership,all").unwrap();
    let records = export(&options);
    let raw: Vec<_> =
        records.iter().flat_map(|record| record["usage_events"].as_array().unwrap()).collect();
    assert_eq!(raw.len(), 11);
    let counted: Vec<_> = raw.iter().filter(|event| event["is_counted"] == true).collect();
    let report =
        build_usage_report(&store, &UsageFilters { sources: None, time_range: TimeRange::All })
            .unwrap();
    assert_eq!(counted.len(), report.summary.events);
    let total: i64 = counted
        .iter()
        .map(|event| {
            [
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
                "reasoning_tokens",
            ]
            .iter()
            .map(|field| event[field].as_i64().unwrap())
            .sum::<i64>()
        })
        .sum();
    assert_eq!(total, report.summary.tokens.total_tokens);
    options.session_ids = vec!["codex-b-copy".to_string(), "claude-copy".to_string()];
    for record in export(&options) {
        assert_eq!(record["usage_events"].as_array().unwrap().len(), 1);
        assert_eq!(record["usage_events"][0]["is_counted"], false);
        let destination = setup();
        let text = record.to_string();
        crate::import::import_jsonl(&destination, false, text.as_bytes()).unwrap();
        let report = build_usage_report(
            &destination,
            &UsageFilters { sources: None, time_range: TimeRange::All },
        )
        .unwrap();
        assert_eq!(report.summary.events, 1);
    }
}
