use super::*;

#[test]
fn gemini_parser_plain_conversation() {
    let json = r#"{
        "sessionId": "abc-123",
        "projectHash": "deadbeef",
        "startTime": "2025-11-13T13:48:00.000Z",
        "lastUpdated": "2025-11-13T14:00:00.000Z",
        "messages": [
            {"id": 0, "type": "user", "content": "hello", "timestamp": "2025-11-13T13:48:05.000Z"},
            {"id": 1, "type": "gemini", "content": "hi there", "timestamp": "2025-11-13T13:48:10.000Z"}
        ]
    }"#;

    let session = parse_gemini_session(json, "fallback").unwrap().unwrap();
    assert_eq!(session.source_id, "abc-123");
    assert_eq!(session.directory, None, "gemini has no resolvable cwd");
    assert_eq!(session.messages.len(), 2);
    assert!(matches!(session.messages[0].role, Role::User));
    assert_eq!(session.messages[0].content, "hello");
    assert!(matches!(session.messages[1].role, Role::Assistant));
    assert_eq!(session.messages[1].content, "hi there");
}

#[test]
fn gemini_parser_indexes_tool_calls() {
    let json = r##"{
        "sessionId": "xyz",
        "startTime": "2025-11-13T13:48:00.000Z",
        "messages": [
            {"id": 0, "type": "user", "content": "read README", "timestamp": "2025-11-13T13:48:00.000Z"},
            {
                "id": 1,
                "type": "gemini",
                "content": "Let me read the file.",
                "timestamp": "2025-11-13T13:48:05.000Z",
                "toolCalls": [{
                    "id": "t1",
                    "name": "read_file",
                    "args": {"path": "/tmp/README.md"},
                    "result": [{"text": "# My Project\nHello world."}]
                }]
            }
        ]
    }"##;

    let session = parse_gemini_session(json, "fallback").unwrap().unwrap();
    let assistant = &session.messages[1];
    assert!(
        assistant.content.contains("Let me read the file"),
        "prose preserved: {}",
        assistant.content
    );
    assert_eq!(assistant.content, "Let me read the file.");
    assert_eq!(session.events.len(), 2);
    assert_eq!(session.events[0].tool_call_id.as_deref(), Some("t1"));
    assert_eq!(session.events[1].tool_call_id, session.events[0].tool_call_id);
    let attrs: serde_json::Value =
        serde_json::from_str(session.events[1].attrs_json.as_deref().unwrap()).unwrap();
    assert_eq!(attrs["toolCalls"][0]["result"][0]["text"], "# My Project\nHello world.");
}

#[test]
fn gemini_parser_skips_info_messages() {
    let json = r#"{
        "sessionId": "s",
        "startTime": "2025-11-13T13:48:00.000Z",
        "messages": [
            {"id": 0, "type": "info", "content": "CLI update available"},
            {"id": 1, "type": "user", "content": "hi", "timestamp": "2025-11-13T13:48:05.000Z"}
        ]
    }"#;

    let session = parse_gemini_session(json, "fallback").unwrap().unwrap();
    assert_eq!(session.messages.len(), 1, "info messages should be skipped");
    assert_eq!(session.messages[0].content, "hi");
}

#[test]
fn gemini_parser_empty_returns_none() {
    let json = r#"{"sessionId": "s", "messages": []}"#;
    assert!(parse_gemini_session(json, "fallback").unwrap().is_none());
}

#[test]
fn kimi_parser_filters_injections_and_stream_parts() {
    let state = r#"{"id":"session_k1","cwd":"/repo","title":"first ask","isCustomTitle":false}"#;
    let wire = concat!(
        r#"{"type":"context.append_message","time":1000,"message":{"id":"m1","role":"user","origin":{"kind":"user"},"content":[{"type":"text","text":"first ask"}]}}"#,
        "\n",
        r#"{"type":"context.append_message","time":1001,"message":{"id":"m2","role":"user","origin":{"kind":"injection"},"content":[{"type":"text","text":"<system-reminder>hidden</system-reminder>"}]}}"#,
        "\n",
        r#"{"type":"context.append_message","time":1002,"message":{"id":"m3","role":"user","origin":{"kind":"task"},"content":[{"type":"text","text":"task notification payload"}]}}"#,
        "\n",
        r#"{"type":"llm.request","time":1003,"model":"kimi-k3","provider":"moonshot"}"#,
        "\n",
        r#"{"type":"context.append_loop_event","time":1004,"event":{"type":"content.part","part":{"type":"think","think":"internal"}}}"#,
        "\n",
        r#"{"type":"context.append_loop_event","time":1005,"event":{"type":"content.part","part":{"type":"text","text":"visible answer"}}}"#,
        "\n",
        r#"{"type":"context.append_loop_event","time":1006,"event":{"type":"tool.result","result":{"output":"secret tool dump"}}}"#,
        "\n",
        r#"{"type":"usage.record","time":1007,"model":"kimi-k3","usage":{"inputOther":10,"output":5,"inputCacheRead":2,"inputCacheCreation":1}}"#,
    );

    let session = parse_kimi_session(state, wire, "session_k1").unwrap();
    let contents: Vec<&str> = session.messages.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents, ["first ask", "visible answer"]);
    assert!(!contents.iter().any(|c| c.contains("hidden")), "injections excluded");
    assert!(
        !contents.iter().any(|c| c.contains("task notification payload")),
        "task-origin notifications excluded"
    );
    assert!(!contents.iter().any(|c| c.contains("internal")), "think parts excluded");
    assert!(!contents.iter().any(|c| c.contains("secret tool dump")), "tool results excluded");
    assert_eq!(session.usage_events.len(), 1);
    assert_eq!(session.usage_events[0].provider, "moonshot");
    assert_eq!(session.summary.as_deref(), Some("first ask"));
}

#[test]
fn kimi_parser_aborted_wire_returns_none() {
    let state = r#"{"id":"session_k2"}"#;
    let wire = r#"{"type":"metadata","protocol_version":"1.5","created_at":1000}"#;
    assert!(parse_kimi_session(state, wire, "session_k2").is_none());
}

#[test]
fn minimax_parser_reads_nested_usage_and_top_level_tool_results() {
    let transcript = concat!(
        r#"{"message_id":"msg-user","turn_id":"turn-1","message":{"role":"user","content":[{"type":"text","text":"Add the MiniMax Code adapter"}],"timestamp":1000}}"#,
        "\n",
        r#"{"message_id":"msg-assistant","turn_id":"turn-1","message":{"role":"assistant","content":[{"type":"text","text":"Indexing local sessions."},{"type":"toolCall","id":"call-1","name":"read","arguments":{"path":"src/adapters/mod.rs"}}],"api":"anthropic-messages","provider":"minimax","model":"MiniMax-M3","usage":{"input":120,"output":30,"cacheRead":20,"cacheWrite":5},"timestamp":1100,"responseId":"resp-1"}}"#,
        "\n",
        r#"{"message_id":"msg-toolresult","turn_id":"turn-1","message":{"role":"toolResult","toolCallId":"call-1","toolName":"read","content":[{"type":"text","text":"contents"}],"isError":false,"timestamp":1200}}"#,
        "\n",
    );
    let session =
        parse_minimax_transcript("mvs_minimax_test", transcript, None).expect("session parsed");

    assert_eq!(session.messages.len(), 2);
    assert_eq!(session.messages[0].role, Role::User);
    assert_eq!(session.messages[0].content, "Add the MiniMax Code adapter");
    assert_eq!(session.messages[1].role, Role::Assistant);
    assert_eq!(session.messages[1].content, "Indexing local sessions.");

    assert_eq!(session.events.len(), 2);
    assert_eq!(session.events[0].kind, "file_read");
    assert_eq!(session.events[0].target.as_deref(), Some("src/adapters/mod.rs"));
    assert_eq!(session.events[0].tool_call_id.as_deref(), Some("call-1"));
    assert_eq!(session.events[1].kind, "tool_result");
    assert_eq!(session.events[1].status.as_deref(), Some("success"));
    assert_eq!(session.events[1].tool_call_id.as_deref(), Some("call-1"));

    assert_eq!(session.usage_events.len(), 1);
    assert_eq!(session.usage_events[0].event_key, "resp-1");
    assert_eq!(session.usage_events[0].model, "MiniMax-M3");
    assert_eq!(session.usage_events[0].provider, "minimax");
    assert_eq!(session.usage_events[0].input_tokens, 120);
    assert_eq!(session.usage_events[0].output_tokens, 30);
    assert_eq!(session.usage_events[0].cache_read_tokens, 20);
    assert_eq!(session.usage_events[0].cache_write_tokens, 5);
}

#[test]
fn kiro_parser_prompt_and_response() {
    let json = r#"{
        "history": [{
            "user": {
                "content": {"Prompt": {"prompt": "how use skill"}},
                "timestamp": "2026-04-11T00:34:50.549369+08:00"
            },
            "assistant": {
                "Response": {"message_id": "m1", "content": "Skills are markdown files."}
            },
            "request_metadata": {"request_start_timestamp_ms": 1775838890550}
        }]
    }"#;

    let session =
        parse_kiro_conversation("conv1", "/Users/x/proj", json, 1000, 2000).unwrap().unwrap();
    assert_eq!(session.source_id, "conv1");
    assert_eq!(session.directory.as_deref(), Some("/Users/x/proj"));
    assert_eq!(session.started_at, 1000);
    assert_eq!(session.updated_at, Some(2000));
    assert_eq!(session.messages.len(), 2);
    assert_eq!(session.messages[0].content, "how use skill");
    assert_eq!(session.messages[1].content, "Skills are markdown files.");
    assert_eq!(session.messages[1].timestamp, Some(1775838890550));
}

#[test]
fn kiro_parser_assistant_tool_use() {
    let json = r#"{
        "history": [{
            "user": {
                "content": {"Prompt": {"prompt": "analyze project"}}
            },
            "assistant": {
                "ToolUse": {
                    "message_id": "m1",
                    "content": "Let me look around.",
                    "tool_uses": [
                        {"id": "t1", "name": "fs_read", "args": {"operations": [{"mode":"Line","path":"/src"}]}},
                        {"id": "t2", "name": "execute_bash", "args": {"command": "ls"}}
                    ]
                }
            },
            "request_metadata": {"request_start_timestamp_ms": 1775838890550}
        }]
    }"#;

    let session = parse_kiro_conversation("c", "/proj", json, 0, 0).unwrap().unwrap();
    let assistant = &session.messages[1];
    assert!(
        assistant.content.contains("Let me look around"),
        "prose preserved: {}",
        assistant.content
    );
    assert_eq!(assistant.content, "Let me look around.");
    assert_eq!(session.events.len(), 2);
    assert_eq!(session.events[0].tool_call_id.as_deref(), Some("t1"));
    assert_eq!(session.events[0].files[0].path, "/src");
    assert_eq!(session.events[1].tool_call_id.as_deref(), Some("t2"));
    let native: serde_json::Value =
        serde_json::from_str(session.events[1].attrs_json.as_deref().unwrap()).unwrap();
    assert_eq!(native["ToolUse"]["tool_uses"][1]["args"]["command"], "ls");
}

#[test]
fn kiro_parser_tool_use_results_text_and_json() {
    let json = r#"{
        "history": [{
            "user": {
                "content": {
                    "ToolUseResults": {
                        "tool_use_results": [
                            {
                                "tool_use_id": "t1",
                                "content": [{"Text": "file contents here"}]
                            },
                            {
                                "tool_use_id": "t2",
                                "content": [{"Json": {"status": "ok", "rows": 42}}]
                            }
                        ]
                    }
                }
            },
            "assistant": {"Response": {"message_id": "m", "content": "done"}}
        }]
    }"#;

    let session = parse_kiro_conversation("c", "/proj", json, 0, 0).unwrap().unwrap();
    assert_eq!(session.messages.len(), 1);
    assert_eq!(session.messages[0].content, "done");
    assert_eq!(session.events.len(), 2);
    for (event, id) in session.events.iter().zip(["t1", "t2"]) {
        assert_eq!(event.tool_call_id.as_deref(), Some(id));
        assert_eq!(event.message_seq, None);
        let native: serde_json::Value =
            serde_json::from_str(event.attrs_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            native["content"]["ToolUseResults"]["tool_use_results"][0]["content"][0]["Text"],
            "file contents here"
        );
        assert_eq!(
            native["content"]["ToolUseResults"]["tool_use_results"][1]["content"][0]["Json"]["rows"],
            42
        );
    }
}

#[test]
fn kiro_parser_empty_history_returns_none() {
    let json = r#"{"history": []}"#;
    assert!(parse_kiro_conversation("c", "/proj", json, 0, 0).unwrap().is_none());
}

#[test]
fn kiro_v2_parser_prompt_and_assistant_text() {
    let sidecar = r#"{
        "session_id": "790bb539-44be-40bd-85ac-0bb1a3fc6b47",
        "cwd": "/Users/x/proj",
        "created_at": "2026-09-01T17:51:14.500361Z",
        "updated_at": "2026-09-01T17:52:01.874564Z",
        "title": "hello, analyze this project"
    }"#;
    let jsonl = r#"{"version":"v1","kind":"Prompt","data":{"message_id":"p1","content":[{"kind":"text","data":"hello, analyze this project"}],"meta":{"timestamp":1788285089}}}
{"version":"v1","kind":"AssistantMessage","data":{"message_id":"a1","content":[{"kind":"thinking","data":{"text":"plan"}},{"kind":"text","data":"This is Recall."},{"kind":"toolUse","data":{"toolUseId":"t1","name":"fs_read","input":{"operations":[{"mode":"Image","image_paths":["/src/a.png","/src/b.png"]}]}}}]}}
{"version":"v1","kind":"ToolResults","data":{"message_id":"t1","content":[{"kind":"toolResult","data":{"toolUseId":"t1","status":"error","content":[{"kind":"text","data":"secret dump"}]}}]}}
{"version":"v1","kind":"Prompt","data":{"message_id":"p2","content":[{"kind":"text","data":"find a bug"}],"meta":{"timestamp":1788285145}}}
{"version":"v1","kind":"AssistantMessage","data":{"message_id":"a2","content":[{"kind":"text","data":"No bug found."}]}}"#;

    let session =
        parse_kiro_v2_session(jsonl, Some(sidecar), "fallback", 9_000, Some("/tmp/s.jsonl".into()))
            .unwrap()
            .unwrap();
    assert_eq!(session.source_id, "790bb539-44be-40bd-85ac-0bb1a3fc6b47");
    assert_eq!(session.directory.as_deref(), Some("/Users/x/proj"));
    assert_eq!(session.custom_title.as_deref(), Some("hello, analyze this project"));
    assert_eq!(session.source_file_path.as_deref(), Some("/tmp/s.jsonl"));
    assert_eq!(session.updated_at, Some(9_000));
    assert_eq!(session.messages.len(), 4);
    assert_eq!(session.messages[0].content, "hello, analyze this project");
    assert_eq!(session.messages[0].timestamp, Some(1_788_285_089_000));
    assert_eq!(session.messages[1].content, "This is Recall.");
    assert!(!session.messages.iter().any(|message| message.content.contains("secret dump")));
    assert!(!session.messages.iter().any(|message| message.content.contains("[read]")));
    assert_eq!(session.messages[2].content, "find a bug");
    assert_eq!(session.messages[3].content, "No bug found.");
    assert_eq!(session.events.len(), 2);
    assert_eq!(
        session.events[0].files.iter().map(|file| file.path.as_str()).collect::<Vec<_>>(),
        ["/src/a.png", "/src/b.png"]
    );
    assert_eq!(session.events[1].tool_call_id, session.events[0].tool_call_id);
    assert_eq!(session.events[1].status.as_deref(), Some("error"));
    assert!(session.events[1].attrs_json.as_ref().unwrap().contains("secret dump"));
}

#[test]
fn kiro_v2_parser_empty_jsonl_returns_none() {
    assert!(parse_kiro_v2_session("", None, "id", 1, None).unwrap().is_none());
}

#[test]
fn kiro_v3_parser_user_and_say_skips_reasoning() {
    let sidecar = r#"{
        "id": "sess_90e28400-458e-47f0-8793-70137f0c92c5",
        "title": "Analyze Recall architecture",
        "createdAt": "2026-09-01T17:52:52.430Z",
        "lastModifiedAt": "2026-09-01T17:56:06.507Z",
        "workspacePaths": ["/Users/x/proj"],
        "rootPaths": ["/Users/x/proj"],
        "modelId": "auto"
    }"#;
    let jsonl = r#"{"id":"u1","timestamp":"2026-09-01T17:52:55.268Z","payload":{"type":"user","content":"analyze the current project","images":[],"documents":[]}}
{"id":"r1","timestamp":"2026-09-01T17:53:01.908Z","payload":{"type":"assistant","content":"...","operationType":"Reasoning"}}
{"id":"t1","timestamp":"2026-09-01T17:53:02.000Z","payload":{"type":"tool_call","toolName":"fs_read","toolCallId":"native-call"}}
{"id":"t2","timestamp":"2026-09-01T17:53:03.000Z","payload":{"type":"tool_result","toolCallId":"native-call","success":false,"content":"file dump"}}
{"id":"a1","timestamp":"2026-09-01T17:56:06.468Z","payload":{"type":"assistant","content":"Recall indexes local sessions.","operationType":"Say"}}
{"id":"s1","timestamp":"2026-09-01T17:56:06.506Z","payload":{"type":"session_start","content":"You are Kiro CLI"}}"#;

    let session = parse_kiro_v3_session(
        jsonl,
        Some(sidecar),
        "sess_fallback",
        9_000,
        Some("/tmp/messages.jsonl".into()),
    )
    .unwrap()
    .unwrap();
    assert_eq!(session.source_id, "sess_90e28400-458e-47f0-8793-70137f0c92c5");
    assert_eq!(session.directory.as_deref(), Some("/Users/x/proj"));
    assert_eq!(session.custom_title.as_deref(), Some("Analyze Recall architecture"));
    assert_eq!(session.updated_at, Some(9_000));
    assert_eq!(session.messages.len(), 2);
    assert_eq!(session.messages[0].content, "analyze the current project");
    assert_eq!(session.messages[1].content, "Recall indexes local sessions.");
    assert!(!session.messages.iter().any(|message| message.content.contains("You are Kiro")));
    assert!(!session.messages.iter().any(|message| message.content.contains("file dump")));
    assert_eq!(session.events.len(), 2);
    assert_eq!(session.events[0].tool_call_id.as_deref(), Some("native-call"));
    assert!(session.events[0].files.is_empty());
    assert_eq!(session.events[1].tool_call_id, session.events[0].tool_call_id);
    assert_eq!(session.events[1].status.as_deref(), Some("error"));
    assert!(session.events[1].attrs_json.as_ref().unwrap().contains("file dump"));
}

#[test]
fn kiro_v3_parser_empty_returns_none() {
    let jsonl = r#"{"id":"r1","timestamp":"2026-09-01T17:53:01.908Z","payload":{"type":"assistant","content":"...","operationType":"Reasoning"}}"#;
    assert!(parse_kiro_v3_session(jsonl, None, "id", 1, None).unwrap().is_none());
}

#[test]
fn copilot_parser_plain_conversation() {
    let jsonl = r#"{"type":"session.start","data":{"sessionId":"sess-1","startTime":"2026-02-26T06:29:59.692Z","context":{"cwd":"/Users/x/proj","repository":"x/proj","branch":"main"}},"id":"e1","timestamp":"2026-02-26T06:29:59.802Z","parentId":null}
{"type":"user.message","data":{"content":"how do I run tests","transformedContent":"wrapped","attachments":[]},"id":"e2","timestamp":"2026-02-26T06:30:00.000Z","parentId":"e1"}
{"type":"assistant.message","data":{"messageId":"m1","content":"Run make check","toolRequests":[]},"id":"e3","timestamp":"2026-02-26T06:30:01.000Z","parentId":"e2"}"#;

    let session = parse_copilot_events(jsonl, "fallback").unwrap().unwrap();
    assert_eq!(session.source_id, "sess-1");
    assert_eq!(session.directory.as_deref(), Some("/Users/x/proj"));
    assert_eq!(session.messages.len(), 2);
    assert!(matches!(session.messages[0].role, Role::User));
    assert_eq!(session.messages[0].content, "how do I run tests");
    assert!(matches!(session.messages[1].role, Role::Assistant));
    assert_eq!(session.messages[1].content, "Run make check");
}

#[test]
fn copilot_parser_indexes_tool_requests_and_results() {
    let jsonl = r##"{"type":"session.start","data":{"sessionId":"sess-2","startTime":"2026-02-26T06:29:59.692Z","context":{"cwd":"/proj"}},"id":"e1","timestamp":"2026-02-26T06:29:59.802Z","parentId":null}
{"type":"assistant.message","data":{"messageId":"m1","content":"Let me read the file.","toolRequests":[{"toolCallId":"tc1","name":"read_file","arguments":{"path":"/tmp/README.md"},"type":"function"}]},"id":"e2","timestamp":"2026-02-26T06:30:00.000Z","parentId":"e1"}
{"type":"tool.execution_start","data":{"toolCallId":"tc1","toolName":"read_file","arguments":{"path":"/tmp/README.md"}},"id":"e3","timestamp":"2026-02-26T06:30:00.100Z","parentId":"e2"}
{"type":"tool.execution_complete","data":{"toolCallId":"tc1","success":true,"result":{"content":"short summary","detailedContent":"# My Project\nHello world."}},"id":"e4","timestamp":"2026-02-26T06:30:00.500Z","parentId":"e3"}"##;

    let session = parse_copilot_events(jsonl, "fallback").unwrap().unwrap();
    assert_eq!(session.messages.len(), 1);
    assert_eq!(session.messages[0].content, "Let me read the file.");
    assert_eq!(session.events.len(), 3);
    assert_eq!(session.events[2].summary.as_deref(), Some("# My Project\nHello world."));
    for (event, record) in session.events.iter().zip(jsonl.lines().skip(1)) {
        assert_eq!(event.tool_call_id.as_deref(), Some("tc1"));
        assert_eq!(event.message_seq, Some(0));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(event.attrs_json.as_deref().unwrap())
                .unwrap(),
            serde_json::from_str::<serde_json::Value>(record).unwrap()
        );
    }
}

#[test]
fn copilot_parser_skips_empty_and_unknown() {
    let jsonl = r#"{"type":"session.start","data":{"sessionId":"s","startTime":"2026-02-26T06:29:59.692Z","context":{"cwd":"/p"}},"id":"e1","timestamp":"2026-02-26T06:29:59.802Z"}
{"type":"session.info","data":{"msg":"anything"},"id":"e2","timestamp":"2026-02-26T06:30:00.000Z"}
{"type":"user.message","data":{"content":"   "},"id":"e3","timestamp":"2026-02-26T06:30:01.000Z"}
{"type":"assistant.message","data":{"messageId":"m","content":"","toolRequests":[]},"id":"e4","timestamp":"2026-02-26T06:30:02.000Z"}
{"type":"user.message","data":{"content":"real question"},"id":"e5","timestamp":"2026-02-26T06:30:03.000Z"}"#;

    let session = parse_copilot_events(jsonl, "fallback").unwrap().unwrap();
    assert_eq!(session.messages.len(), 1, "empty and unknown events should be skipped");
    assert_eq!(session.messages[0].content, "real question");
}

#[test]
fn copilot_parser_empty_returns_none() {
    let jsonl = r#"{"type":"session.start","data":{"sessionId":"s","startTime":"2026-02-26T06:29:59.692Z"},"id":"e1","timestamp":"2026-02-26T06:29:59.802Z"}"#;
    assert!(parse_copilot_events(jsonl, "fallback").unwrap().is_none());
}

#[test]
fn copilot_parser_falls_back_to_dir_id_when_session_missing() {
    let jsonl = r#"{"type":"user.message","data":{"content":"hi"},"id":"e1","timestamp":"2026-02-26T06:30:00.000Z"}"#;
    let session = parse_copilot_events(jsonl, "dir-uuid").unwrap().unwrap();
    assert_eq!(session.source_id, "dir-uuid");
}
