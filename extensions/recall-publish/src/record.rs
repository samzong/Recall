use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

use crate::manifest::{DATASET_SCHEMA_NAME, DATASET_SCHEMA_VERSION, SOURCE_SCHEMA_VERSION};

const NESTED_JSON_KEYS: [&str; 2] = ["attrs_json", "raw_usage_json"];

const STRUCTURAL_KEYS: [&str; 16] = [
    "id",
    "source",
    "source_id",
    "source_event_id",
    "tool_call_id",
    "event_key",
    "role",
    "kind",
    "actor",
    "thread_role",
    "relation",
    "operation",
    "token_source",
    "visibility",
    "record_type",
    "command_evidence_status",
];

pub struct ExportRecord {
    pub session_id: String,
    pub started_at: i64,
    pub source: String,
    pub value: Value,
}

pub fn parse_export_record(line: &str, line_number: usize) -> Result<ExportRecord> {
    let value: Value = serde_json::from_str(line)
        .map_err(|error| anyhow::anyhow!("export record {line_number} is not JSON: {error}"))?;
    let schema_version = value.get("schema_version").and_then(Value::as_u64);
    if schema_version != Some(SOURCE_SCHEMA_VERSION as u64) {
        bail!(
            "export record {line_number} has schema_version {:?}; recall-publish requires {SOURCE_SCHEMA_VERSION}",
            schema_version
        );
    }
    if value.get("record_type").and_then(Value::as_str) != Some("session") {
        bail!("export record {line_number} is not a session record");
    }
    let session = value
        .get("session")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("export record {line_number} has no session object"))?;
    let session_id = session
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("export record {line_number} has no session id"))?
        .to_string();
    let started_at = session
        .get("started_at")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow::anyhow!("session {session_id} has no started_at timestamp"))?;
    let source = session.get("source").and_then(Value::as_str).unwrap_or_default().to_string();
    Ok(ExportRecord { session_id, started_at, source, value })
}

pub fn to_public_record(record: &ExportRecord) -> Result<Value> {
    let object = record.value.as_object().expect("export record is an object");
    let mut session = object.get("session").and_then(Value::as_object).cloned().unwrap_or_default();

    session.remove("directory");
    session.remove("source_file_path");
    let remote = session.remove("repo_remote").unwrap_or(Value::Null);
    let slug = session.remove("repo_slug").unwrap_or(Value::Null);
    let name = session.remove("repo_name").unwrap_or(Value::Null);
    let repo = if remote.is_null() && slug.is_null() && name.is_null() {
        Value::Null
    } else {
        json!({ "remote": remote, "slug": slug, "name": name })
    };
    session.insert("repo".to_string(), repo);

    let mut usage_events = array_of(object, "usage_events");
    for event in &mut usage_events {
        if let Some(event) = event.as_object_mut() {
            event.remove("source_path");
        }
    }

    let mut events = array_of(object, "events");
    for event in &mut events {
        let Some(event) = event.as_object_mut() else { continue };
        event.remove("source_path");
        let Some(files) = event.get_mut("files").and_then(Value::as_array_mut) else { continue };
        for file in files {
            let Some(file) = file.as_object_mut() else { continue };
            file.remove("cwd");
            if let Some(target) = file.get_mut("target").and_then(Value::as_object_mut) {
                target.remove("absolute_path");
                target.remove("repo_root");
            }
        }
    }

    let mut unavailable = Vec::new();
    if object.get("usage_events").is_none() {
        unavailable.push(Value::String("usage_events".to_string()));
    }
    if object.get("events").is_none() {
        unavailable.push(Value::String("events".to_string()));
    }

    Ok(json!({
        "schema": { "name": DATASET_SCHEMA_NAME, "version": DATASET_SCHEMA_VERSION },
        "source_schema_version": SOURCE_SCHEMA_VERSION,
        "session": Value::Object(session),
        "messages": array_of(object, "messages"),
        "usage_events": usage_events,
        "events": events,
        "redaction": { "status": "clean", "spans": 0, "entities": {}, "fields": [] },
        "unavailable": unavailable,
    }))
}

fn array_of(object: &Map<String, Value>, key: &str) -> Vec<Value> {
    object.get(key).and_then(Value::as_array).cloned().unwrap_or_default()
}

pub fn escape_pointer_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

pub fn visit_text_leaves<F>(value: &mut Value, visit: &mut F) -> Result<bool>
where
    F: FnMut(&str, &str) -> Option<String>,
{
    let mut prefix = String::new();
    visit_inner(value, &mut prefix, None, true, visit)
}

fn visit_inner<F>(
    value: &mut Value,
    prefix: &mut String,
    key: Option<&str>,
    structural: bool,
    visit: &mut F,
) -> Result<bool>
where
    F: FnMut(&str, &str) -> Option<String>,
{
    match value {
        Value::String(text) => {
            let Some(key) = key else { return Ok(false) };
            if structural && STRUCTURAL_KEYS.contains(&key) {
                return Ok(false);
            }
            if NESTED_JSON_KEYS.contains(&key)
                && let Ok(mut nested) = serde_json::from_str::<Value>(text)
                && !nested.is_string()
            {
                let changed = visit_inner(&mut nested, prefix, Some("__nested__"), false, visit)?;
                if changed {
                    *text = serde_json::to_string(&nested)?;
                }
                return Ok(changed);
            }
            match visit(prefix, text) {
                Some(replacement) if replacement != *text => {
                    *text = replacement;
                    Ok(true)
                }
                _ => Ok(false),
            }
        }
        Value::Array(items) => {
            let mut changed = false;
            for (index, item) in items.iter_mut().enumerate() {
                let length = prefix.len();
                prefix.push('/');
                prefix.push_str(&index.to_string());
                changed |= visit_inner(item, prefix, key, structural, visit)?;
                prefix.truncate(length);
            }
            Ok(changed)
        }
        Value::Object(entries) => {
            let mut changed = false;
            for (entry_key, entry) in entries.iter_mut() {
                let length = prefix.len();
                prefix.push('/');
                prefix.push_str(&escape_pointer_token(entry_key));
                changed |= visit_inner(entry, prefix, Some(entry_key.as_str()), structural, visit)?;
                prefix.truncate(length);
            }
            Ok(changed)
        }
        _ => Ok(false),
    }
}

pub fn collect_text_leaves(value: &Value) -> Result<Vec<(String, String)>> {
    let mut leaves = Vec::new();
    let mut clone = value.clone();
    visit_text_leaves(&mut clone, &mut |pointer, text| {
        if !text.is_empty() {
            leaves.push((pointer.to_string(), text.to_string()));
        }
        None
    })?;
    Ok(leaves)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECORD: &str = r#"{"schema_version":7,"record_type":"session","session":{"id":"s1","source":"codex","source_id":"native-1","title":"Fix sync","directory":"/Users/x/git/Recall","repo_remote":"github.com/samzong/Recall","repo_slug":"samzong/Recall","repo_name":"Recall","started_at":1756684800000,"updated_at":1756688400000,"message_count":1,"entrypoint":"cli","custom_title":null,"summary":null,"duration_minutes":3,"source_file_path":"/Users/x/.codex/sessions/a.jsonl","topology":{"thread_role":"primary","parents":[]}},"messages":[{"seq":0,"role":"user","timestamp":1756684800000,"content":"hello"}],"usage_events":[{"event_key":"u1","event_seq":0,"message_seq":0,"timestamp":1756684800000,"model":"gpt","provider":"openai","input_tokens":1,"output_tokens":1,"cache_read_tokens":0,"cache_write_tokens":0,"reasoning_tokens":0,"token_source":"observed","parser_version":1,"source_path":"/Users/x/.codex/sessions/a.jsonl","raw_usage_json":"{\"total\":2}"}],"events":[{"command_evidence_status":null,"files":[{"path":"src/lib.rs","operation":"read","kind":"call","cwd":"/Users/x/git/Recall","target":{"absolute_path":"/Users/x/git/Recall/src/lib.rs","repo_root":"/Users/x/git/Recall","repo_relative_path":"src/lib.rs","repo_remote":"github.com/samzong/Recall"}}],"event_seq":0,"timestamp":1756684800000,"kind":"tool","actor":"assistant","name":"Read","status":null,"target":null,"message_seq":0,"summary":"read lib.rs","source_path":"/Users/x/.codex/sessions/a.jsonl","source_event_id":"e1","tool_call_id":"t1","is_meta":null,"visibility":"visible","attrs_json":"{\"input\":{\"command\":\"cat /Users/x/secret\"}}","parser_version":1}]}"#;

    fn public() -> Value {
        let record = parse_export_record(RECORD, 1).unwrap();
        to_public_record(&record).unwrap()
    }

    #[test]
    fn local_filesystem_locations_are_removed_not_redacted() {
        let public = public();
        let serialized = serde_json::to_string(&public).unwrap();
        assert!(public["session"].get("directory").is_none());
        assert!(public["session"].get("source_file_path").is_none());
        assert!(public["usage_events"][0].get("source_path").is_none());
        assert!(public["events"][0].get("source_path").is_none());
        assert!(public["events"][0]["files"][0].get("cwd").is_none());
        assert!(public["events"][0]["files"][0]["target"].get("absolute_path").is_none());
        assert!(public["events"][0]["files"][0]["target"].get("repo_root").is_none());
        assert!(!serialized.contains("/Users/x/.codex"));
        assert!(!serialized.contains("\"/Users/x/git/Recall\""));
    }

    #[test]
    fn repository_identity_is_grouped_and_retained() {
        let public = public();
        assert_eq!(public["session"]["repo"]["slug"], "samzong/Recall");
        assert_eq!(public["session"]["repo"]["name"], "Recall");
        assert_eq!(public["events"][0]["files"][0]["path"], "src/lib.rs");
        assert_eq!(public["events"][0]["files"][0]["target"]["repo_relative_path"], "src/lib.rs");
    }

    #[test]
    fn schema_version_is_public_and_separate_from_the_export_version() {
        let public = public();
        assert_eq!(public["schema"]["name"], DATASET_SCHEMA_NAME);
        assert_eq!(public["schema"]["version"], 1);
        assert_eq!(public["source_schema_version"], 7);
    }

    #[test]
    fn unsupported_export_schema_versions_are_refused() {
        let older = RECORD.replace("\"schema_version\":7", "\"schema_version\":6");
        assert!(parse_export_record(&older, 1).is_err());
    }

    #[test]
    fn leaves_reach_nested_tool_payloads_but_skip_identifiers() {
        let leaves = collect_text_leaves(&public()).unwrap();
        let pointers: Vec<&str> = leaves.iter().map(|(pointer, _)| pointer.as_str()).collect();
        assert!(pointers.contains(&"/events/0/attrs_json/input/command"));
        assert!(!pointers.contains(&"/usage_events/0/raw_usage_json/total"));
        assert!(pointers.contains(&"/messages/0/content"));
        assert!(pointers.contains(&"/session/title"));
        assert!(!pointers.contains(&"/session/id"));
        assert!(!pointers.contains(&"/session/source"));
        assert!(!pointers.contains(&"/events/0/tool_call_id"));
    }

    #[test]
    fn nested_payload_fields_named_like_identifiers_are_still_scanned() {
        let mut value = serde_json::json!({
            "events": [{"attrs_json": "{\"input\":{\"source\":\"a\",\"id\":\"b\",\"role\":\"c\"}}"}]
        });
        let mut pointers = Vec::new();
        visit_text_leaves(&mut value, &mut |pointer, _| {
            pointers.push(pointer.to_string());
            None
        })
        .unwrap();
        assert!(pointers.iter().any(|pointer| pointer == "/events/0/attrs_json/input/source"));
        assert!(pointers.iter().any(|pointer| pointer == "/events/0/attrs_json/input/id"));
        assert!(pointers.iter().any(|pointer| pointer == "/events/0/attrs_json/input/role"));
    }

    #[test]
    fn redacting_a_nested_payload_keeps_the_field_a_valid_json_string() {
        let mut public = public();
        let changed = visit_text_leaves(&mut public, &mut |pointer, text| {
            (pointer == "/events/0/attrs_json/input/command")
                .then(|| text.replace("/Users/x/secret", "[REDACTED:PATH]"))
        })
        .unwrap();
        assert!(changed);
        let attrs = public["events"][0]["attrs_json"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(attrs).unwrap();
        assert_eq!(parsed["input"]["command"], "cat [REDACTED:PATH]");
    }

    #[test]
    fn unchanged_nested_payloads_keep_their_original_bytes() {
        let mut public = public();
        let before = public["events"][0]["attrs_json"].as_str().unwrap().to_string();
        let changed = visit_text_leaves(&mut public, &mut |_, _| None).unwrap();
        assert!(!changed);
        assert_eq!(public["events"][0]["attrs_json"].as_str().unwrap(), before);
    }
}
