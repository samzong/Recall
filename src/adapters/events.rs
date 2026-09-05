use serde_json::Value;

use crate::types::{FileEvidence, FileEvidenceKind, FileOperation, RawSessionEvent};

mod command;

pub(crate) use command::{command_file_evidence, shell_file_evidence};

const EVENT_SUMMARY_MAX_BYTES: usize = 4096;

pub(crate) struct EventContext {
    pub(crate) event_seq: u32,
    pub(crate) timestamp: Option<i64>,
    pub(crate) source_path: Option<String>,
    pub(crate) source_event_id: Option<String>,
    pub(crate) message_seq: Option<u32>,
    pub(crate) parser_version: u32,
}

pub(crate) fn tool_call_event(
    context: EventContext,
    name: String,
    args: Option<&Value>,
) -> RawSessionEvent {
    let target = args.and_then(target_from_value);
    let kind = infer_tool_kind(&name, target.as_deref());
    let summary = args.map(|value| match value {
        Value::String(text) if text.trim().is_empty() => format!("[{name}]"),
        Value::String(text) => format!("[{name}] {text}"),
        other => format!("[{name}] {other}"),
    });
    RawSessionEvent {
        command_evidence_status: None,
        files: Vec::new(),
        event_seq: context.event_seq,
        timestamp: context.timestamp,
        kind: kind.to_string(),
        actor: "assistant".to_string(),
        name: Some(name),
        status: None,
        target,
        message_seq: context.message_seq,
        summary: summary.map(bounded_summary),
        source_path: context.source_path,
        source_event_id: context.source_event_id,
        tool_call_id: None,
        is_meta: None,
        visibility: None,
        attrs_json: args.map(|value| value.to_string()),
        parser_version: context.parser_version,
    }
}

pub(crate) fn tool_call_event_from_text(
    context: EventContext,
    name: String,
    args: Option<&str>,
) -> RawSessionEvent {
    let parsed = args.and_then(|text| serde_json::from_str::<Value>(text).ok());
    let target =
        parsed.as_ref().and_then(target_from_value).or_else(|| command_target(&name, args));
    let kind = infer_tool_kind(&name, target.as_deref());
    let summary = args.map(|text| {
        if text.trim().is_empty() { format!("[{name}]") } else { format!("[{name}] {text}") }
    });
    RawSessionEvent {
        command_evidence_status: None,
        files: Vec::new(),
        event_seq: context.event_seq,
        timestamp: context.timestamp,
        kind: kind.to_string(),
        actor: "assistant".to_string(),
        name: Some(name),
        status: None,
        target,
        message_seq: context.message_seq,
        summary: summary.map(bounded_summary),
        source_path: context.source_path,
        source_event_id: context.source_event_id,
        tool_call_id: None,
        is_meta: None,
        visibility: None,
        attrs_json: parsed.map(|value| value.to_string()),
        parser_version: context.parser_version,
    }
}

pub(crate) fn tool_result_event(
    context: EventContext,
    name: Option<String>,
    summary: Option<String>,
) -> RawSessionEvent {
    RawSessionEvent {
        command_evidence_status: None,
        files: Vec::new(),
        event_seq: context.event_seq,
        timestamp: context.timestamp,
        kind: "tool_result".to_string(),
        actor: "tool".to_string(),
        name,
        status: None,
        target: None,
        message_seq: context.message_seq,
        summary: summary.map(bounded_summary),
        source_path: context.source_path,
        source_event_id: context.source_event_id,
        tool_call_id: None,
        is_meta: None,
        visibility: None,
        attrs_json: None,
        parser_version: context.parser_version,
    }
}

pub(crate) fn file_write_event(
    context: EventContext,
    name: String,
    target: String,
) -> RawSessionEvent {
    let summary = format!("[{name}] {target}");
    RawSessionEvent {
        command_evidence_status: None,
        files: Vec::new(),
        event_seq: context.event_seq,
        timestamp: context.timestamp,
        kind: "file_write".to_string(),
        actor: "assistant".to_string(),
        name: Some(name),
        status: None,
        target: Some(target),
        message_seq: context.message_seq,
        summary: Some(bounded_summary(summary)),
        source_path: context.source_path,
        source_event_id: context.source_event_id,
        tool_call_id: None,
        is_meta: None,
        visibility: None,
        attrs_json: None,
        parser_version: context.parser_version,
    }
}

pub(crate) fn patch_file_evidence(text: &str) -> Vec<FileEvidence> {
    let text = text.trim();
    if text.lines().next() != Some("*** Begin Patch")
        || text.lines().last() != Some("*** End Patch")
    {
        return Vec::new();
    }
    let mut files: Vec<FileEvidence> = Vec::new();
    let mut update = None;
    for line in text.lines() {
        let parsed = if let Some(path) = line.strip_prefix("*** Update File: ") {
            update = Some(files.len());
            Some((path, FileOperation::Write))
        } else if let Some(path) = line.strip_prefix("*** Add File: ") {
            update = None;
            Some((path, FileOperation::Write))
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            update = None;
            Some((path, FileOperation::Delete))
        } else if let Some(path) =
            line.strip_prefix("*** Move to: ").filter(|path| !path.trim().is_empty())
        {
            if let Some(index) = update.take() {
                if let Some(file) = files.get_mut(index) {
                    file.operation = FileOperation::MoveFrom;
                    Some((path, FileOperation::MoveTo))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        if let Some((path, operation)) = parsed
            && !path.trim().is_empty()
        {
            files.push(FileEvidence {
                path: path.into(),
                operation,
                kind: FileEvidenceKind::Call,
                cwd: None,
                target: None,
            });
        }
    }
    files
}

pub(crate) fn target_from_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => non_empty(text),
        Value::Array(values) => command_target_from_array(values),
        Value::Object(map) => {
            for key in [
                "path",
                "file_path",
                "filePath",
                "target",
                "command",
                "cmd",
                "query",
                "pattern",
                "glob",
                "glob_pattern",
                "regex",
                "url",
            ] {
                if let Some(text) = map.get(key).and_then(target_from_value) {
                    return Some(text);
                }
            }
            None
        }
        _ => None,
    }
}

fn command_target_from_array(values: &[Value]) -> Option<String> {
    let parts: Option<Vec<&str>> = values.iter().map(|value| value.as_str()).collect();
    let parts = parts?;
    if parts.is_empty() {
        return None;
    }
    if parts.len() >= 3
        && matches!(parts[0], "bash" | "sh" | "zsh")
        && matches!(parts[1], "-c" | "-lc")
    {
        return non_empty(parts[2]);
    }
    non_empty(&parts.join(" "))
}

fn command_target(name: &str, args: Option<&str>) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    if lower.contains("bash") || lower.contains("shell") || lower.contains("exec") {
        return args.and_then(non_empty);
    }
    None
}

fn infer_tool_kind(name: &str, target: Option<&str>) -> &'static str {
    let lower = name.to_ascii_lowercase();
    if lower.contains("bash") || lower.contains("shell") || lower.contains("exec") {
        return "command";
    }
    if lower.contains("grep")
        || lower.contains("search")
        || lower.contains("glob")
        || lower.contains("find")
    {
        return "search";
    }
    if target.is_some()
        && (lower.contains("edit")
            || lower.contains("write")
            || lower.contains("patch")
            || lower.contains("delete"))
    {
        return "file_write";
    }
    if target.is_some()
        && (lower.contains("read") || lower.contains("open") || lower.contains("view"))
    {
        return "file_read";
    }
    "tool_call"
}

fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

pub(crate) fn bounded_summary(summary: String) -> String {
    if summary.len() <= EVENT_SUMMARY_MAX_BYTES {
        return summary;
    }
    let mut end = EVENT_SUMMARY_MAX_BYTES;
    while !summary.is_char_boundary(end) {
        end -= 1;
    }
    let mut capped = summary[..end].to_string();
    capped.push('…');
    capped
}

#[cfg(test)]
mod tests {
    use super::{EVENT_SUMMARY_MAX_BYTES, EventContext, tool_result_event};

    fn context() -> EventContext {
        EventContext {
            event_seq: 0,
            timestamp: None,
            source_path: None,
            source_event_id: None,
            message_seq: None,
            parser_version: 1,
        }
    }

    #[test]
    fn patch_headers_preserve_context_and_incomplete_moves() {
        let files = super::patch_file_evidence(
            "*** Begin Patch\r\n*** Update File: src/main.rs\r\n*** Move to:   \r\n@@\r\n *** Update File: example.rs\r\n*** End Patch",
        );
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/main.rs");
        assert_eq!(files[0].operation, crate::types::FileOperation::Write);
    }

    #[test]
    fn tool_result_summary_is_capped_on_a_char_boundary() {
        let long = "汉".repeat(EVENT_SUMMARY_MAX_BYTES);
        let event = tool_result_event(context(), None, Some(long));
        let summary = event.summary.unwrap();
        assert!(summary.ends_with('…'));
        assert!(summary.len() <= EVENT_SUMMARY_MAX_BYTES + '…'.len_utf8());
        assert!(summary.trim_end_matches('…').chars().all(|c| c == '汉'));

        let short = "ok".to_string();
        let event = tool_result_event(context(), None, Some(short.clone()));
        assert_eq!(event.summary, Some(short));
    }

    #[test]
    fn command_candidates_decode_literals_without_inventing_execution() {
        use crate::types::{CommandEvidenceStatus, FileEvidenceKind};
        let patch = "*** Begin Patch\n*** Update File: /repo/README.zh-CN.md\n@@\n-old\n+new\n*** End Patch";
        for script in [
            format!("text(await tools.apply_patch({}));", serde_json::to_string(patch).unwrap()),
            format!(
                "const patch = {};\ntext(await tools.apply_patch(patch));",
                serde_json::to_string(patch).unwrap()
            ),
            format!("if (false) {{ await tools.apply_patch(`{patch}`); }}"),
            format!("text({});", serde_json::to_string(patch).unwrap()),
            format!("const patch = {} + suffix;", serde_json::to_string(patch).unwrap()),
        ] {
            let (files, status) =
                super::command_file_evidence("exec", Some(&serde_json::json!(script)), None);
            assert_eq!(status, CommandEvidenceStatus::Unsupported);
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].path, "/repo/README.zh-CN.md");
            assert_eq!(files[0].kind, FileEvidenceKind::Command);
            assert!(files[0].cwd.is_none());
        }
        for script in [
            format!(
                "text({});",
                serde_json::to_string(&format!("tools.apply_patch({patch:?})")).unwrap()
            ),
            format!("/* tools.apply_patch(`{patch}`) */"),
        ] {
            assert!(
                super::command_file_evidence("exec", Some(&serde_json::json!(script)), None)
                    .0
                    .is_empty()
            );
        }
        let (files, status) = super::command_file_evidence(
            "exec",
            Some(&serde_json::json!(
                "await tools.apply_patch(`*** Begin Patch\n*** Update File: ${file}\n*** End Patch`);"
            )),
            None,
        );
        assert!(files.is_empty());
        assert_eq!(status, CommandEvidenceStatus::Unsupported);
    }

    #[test]
    fn command_candidates_keep_directory_and_heredoc_boundaries() {
        use crate::types::{CommandEvidenceStatus, FileOperation};
        let command = "git restore --staged -- index-only\ngit restore -- 'file one'\ncd /second && mv -- old new\ngit checkout -- ambiguous\napply_patch <<'PATCH'\n*** Begin Patch\n*** Delete File: /repo/deleted\n*** Add File: /repo/added\n+${literal}\n*** End Patch\nPATCH\ncat <<'QUOTE'\ngit restore -- not-executed\nQUOTE";
        let script = format!(
            "await tools.exec_command({{cmd:{},workdir:'/first',yield_time_ms:1000}});",
            serde_json::to_string(command).unwrap()
        );
        let (files, status) =
            super::command_file_evidence("functions.exec", Some(&serde_json::json!(script)), None);
        assert_eq!(status, CommandEvidenceStatus::Unsupported);
        assert_eq!(
            files.iter().map(|file| file.path.as_str()).collect::<Vec<_>>(),
            ["file one", "old", "new", "ambiguous", "/repo/deleted", "/repo/added"]
        );
        assert_eq!(files[0].cwd.as_deref(), Some("/first"));
        assert_eq!(files[1].cwd.as_deref(), Some("/second"));
        assert_eq!(files[1].operation, FileOperation::MoveFrom);
        assert_eq!(files[2].operation, FileOperation::MoveTo);
        assert!(files[3].cwd.is_none());
        assert_eq!(files[4].operation, FileOperation::Delete);
        let (files, status) = super::command_file_evidence(
            "exec",
            Some(&serde_json::json!(
                "tools.exec_command({cmd:'git checkout -- relative', workdir: destination});"
            )),
            None,
        );
        assert_eq!(status, CommandEvidenceStatus::Unsupported);
        assert_eq!(files.len(), 1);
        assert!(files[0].cwd.is_none());
        let (files, status) = super::command_file_evidence(
            "exec_command",
            Some(
                &serde_json::json!({"cmd":"printf 'git restore -- absent'\ngit -C /target restore -- file", "workdir":"/session"}),
            ),
            None,
        );
        assert_eq!(status, CommandEvidenceStatus::Complete);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].cwd.as_deref(), Some("/target"));
    }

    #[test]
    fn heredoc_target_is_distinct_from_printed_command_text() {
        let command = "cat > scripts/regenerate-changelog <<'HOOK'\n#!/bin/sh\ngit restore -- printed-only\nHOOK\ncat <<'TEXT'\ncat > not-written <<INNER\nTEXT";
        let (files, _) = super::shell_file_evidence(command, Some("/repo"));
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "scripts/regenerate-changelog");
        assert_eq!(files[0].cwd.as_deref(), Some("/repo"));
        assert_eq!(files[0].kind, crate::types::FileEvidenceKind::Command);
        let (files, _) = super::command_file_evidence(
            "exec_command",
            Some(&serde_json::json!({"cmd":"git restore -- tracked"})),
            Some("/native-turn"),
        );
        assert_eq!(files[0].cwd.as_deref(), Some("/native-turn"));
        let (files, status) = super::shell_file_evidence(
            "python <<'PY'\nignored()\nPY\ngit restore -- uncertain",
            Some("/repo"),
        );
        assert_eq!(status, crate::types::CommandEvidenceStatus::Unsupported);
        assert_eq!(files.len(), 1);
        assert!(files[0].cwd.is_none());
        for command in [
            "apply_patch <<'PATCH' 2>/dev/null\n*** Begin Patch\n*** Update File: /repo/a.rs\n@@\n-old\n+new\n*** End Patch\nPATCH",
            "git restore -- ' '",
        ] {
            let (files, status) = super::shell_file_evidence(command, Some("/repo"));
            assert_eq!(status, crate::types::CommandEvidenceStatus::Unsupported);
            assert!(files.iter().all(|file| !file.path.trim().is_empty()));
        }
    }

    #[test]
    fn command_scan_limits_are_explicit() {
        use crate::types::CommandEvidenceStatus;
        let script = " ".repeat(1_048_577);
        let (files, status) =
            super::command_file_evidence("exec", Some(&serde_json::json!(script)), None);
        assert!(files.is_empty());
        assert_eq!(status, CommandEvidenceStatus::LimitExceeded);
        let (files, status) = super::command_file_evidence(
            "exec",
            Some(&serde_json::json!("a;".repeat(16_385))),
            None,
        );
        assert!(files.is_empty());
        assert_eq!(status, CommandEvidenceStatus::LimitExceeded);
        let patch = format!(
            "*** Begin Patch\n{}*** End Patch",
            (0..257).map(|i| format!("*** Add File: /repo/{i}\n+x\n")).collect::<String>()
        );
        let script = format!("tools.apply_patch({});", serde_json::to_string(&patch).unwrap());
        let (files, status) =
            super::command_file_evidence("exec", Some(&serde_json::json!(script)), None);
        assert_eq!(files.len(), 256);
        assert_eq!(status, CommandEvidenceStatus::LimitExceeded);
    }
}
