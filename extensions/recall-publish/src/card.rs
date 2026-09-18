use serde_json::Value;

pub fn render(scope: &str, data_file: &str, manifest: &Value) -> String {
    let license = manifest["license"].as_str().unwrap_or("other");
    let sessions = manifest["files"][0]["sessions"].as_u64().unwrap_or(0);
    let author = manifest["publisher"]["author"].as_str().unwrap_or("unknown");
    let project = manifest["selection"]["project"].as_str().unwrap_or("unknown");

    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("license: {}\n", license.to_ascii_lowercase()));
    out.push_str("language:\n- en\n- zh\n");
    out.push_str("task_categories:\n- text-generation\n");
    out.push_str("tags:\n- coding-sessions\n- agent-transcripts\n- recall\n");
    out.push_str(&format!("size_categories:\n- {}\n", size_category(sessions)));
    out.push_str("configs:\n- config_name: default\n  data_files:\n  - split: train\n    path: ");
    out.push_str(data_file);
    out.push_str("\n---\n\n");

    out.push_str(&format!("# {scope}\n\n"));
    out.push_str(&format!(
        "Local AI coding sessions from the `{project}` project, exported and redacted with \
         [Recall](https://github.com/samzong/Recall) and published by `{author}`.\n\n"
    ));

    out.push_str("## Selection\n\n");
    let time = &manifest["selection"]["time"];
    if let (Some(since), Some(until)) = (time["since"].as_str(), time["until"].as_str()) {
        out.push_str(&format!("- Window: `{since}` to `{until}` on `session.started_at`\n"));
    }
    out.push_str(&format!("- Sessions: {sessions}\n"));
    out.push_str(&format!("- Sources: {}\n", list(&manifest["selection"]["sources"], "all")));
    out.push_str(&format!(
        "- Thread roles: {}\n\n",
        list(&manifest["selection"]["thread_roles"], "all")
    ));

    out.push_str("## Files\n\n");
    out.push_str(&format!(
        "- `{data_file}` — one JSON object per session, Recall export schema version {}\n",
        manifest["producer"]["source_schema_version"]
    ));
    out.push_str(
        "- `manifest.json` — selection, file digests, and the full redaction record\n\n\
         Timestamps are Unix epoch milliseconds in UTC.\n\n",
    );

    out.push_str("## Redaction\n\n");
    out.push_str(
        "Every text field was scanned for secrets with gitleaks and for identities with \
         Presidio before publication, and the redacted records were rescanned; a surviving \
         secret blocks publication.\n\n",
    );
    match manifest["redaction"]["entities"].as_object() {
        Some(entities) if !entities.is_empty() => {
            out.push_str("Replaced spans:\n\n");
            for (name, count) in entities {
                out.push_str(&format!("- `{name}`: {count}\n"));
            }
            out.push('\n');
        }
        _ => out.push_str("No identity spans were replaced.\n\n"),
    }
    out.push_str("### Known limits\n\n");
    if let Some(notes) = manifest["redaction"]["coverage_notes"].as_array() {
        for note in notes.iter().filter_map(Value::as_str) {
            out.push_str(&format!("- {note}\n"));
        }
    }
    out.push_str("\nRedaction is automated and imperfect. Read the data before relying on it.\n\n");

    out.push_str("### Tools\n\n");
    if let Some(tools) = manifest["redaction"]["tools"].as_object() {
        for (name, version) in tools {
            out.push_str(&format!("- {name} {}\n", version.as_str().unwrap_or("")));
        }
    }

    out.push_str(&format!("\n## License\n\n{license}\n"));
    out
}

fn list(value: &Value, empty: &str) -> String {
    match value.as_array() {
        Some(items) if !items.is_empty() => items
            .iter()
            .filter_map(Value::as_str)
            .map(|item| format!("`{item}`"))
            .collect::<Vec<_>>()
            .join(", "),
        _ => empty.to_string(),
    }
}

fn size_category(sessions: u64) -> &'static str {
    match sessions {
        0..=999 => "n<1K",
        1_000..=9_999 => "1K<n<10K",
        _ => "10K<n<100K",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest() -> Value {
        json!({
            "license": "CC-BY-4.0",
            "publisher": { "author": "samzong" },
            "selection": {
                "project": "Recall",
                "sources": [],
                "thread_roles": [],
                "time": { "since": "2026-09-12T00:00:00+00:00", "until": "2026-09-13T00:00:00+00:00" }
            },
            "files": [{ "sessions": 1 }],
            "producer": { "source_schema_version": 7 },
            "redaction": {
                "entities": { "EMAIL_ADDRESS": 8 },
                "coverage_notes": ["Entity types not redacted: PERSON, LOCATION."],
                "tools": { "gitleaks": "8.30.1" }
            }
        })
    }

    #[test]
    fn the_viewer_config_points_at_the_published_data_file() {
        let card = render("scope-a", "scope-a.recall.jsonl", &manifest());
        assert!(card.starts_with("---\n"));
        assert!(card.contains("    path: scope-a.recall.jsonl\n"));
        assert!(card.contains("license: cc-by-4.0\n"));
    }

    #[test]
    fn the_card_states_which_entities_stay_in_the_data() {
        let card = render("scope-a", "scope-a.recall.jsonl", &manifest());
        assert!(card.contains("Entity types not redacted: PERSON, LOCATION."));
        assert!(card.contains("`EMAIL_ADDRESS`: 8"));
    }
}
