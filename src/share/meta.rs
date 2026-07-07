use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::types::{Session, SessionUsageEventRecord};

#[derive(Debug, Default, Clone)]
pub(crate) struct SessionDisplayMeta {
    pub(crate) models: Vec<String>,
    pub(crate) thinking_depths: Vec<String>,
}

pub(crate) fn collect_session_display_meta(
    session: &Session,
    usage_events: &[SessionUsageEventRecord],
) -> SessionDisplayMeta {
    let mut meta = SessionDisplayMeta::default();
    enrich_display_meta_from_usage(&mut meta, usage_events);
    if let Err(err) = enrich_display_meta_from_source(session, &mut meta) {
        tracing::debug!("session display meta source enrichment skipped: {err}");
    }
    meta
}

fn enrich_display_meta_from_usage(
    meta: &mut SessionDisplayMeta,
    usage_events: &[SessionUsageEventRecord],
) {
    for event in usage_events {
        if event.model != "unknown" {
            push_unique(&mut meta.models, &event.model);
        }
        if let Some(raw) = event.raw_usage_json.as_deref() {
            enrich_display_meta_from_json_text(meta, raw);
        }
    }
}

fn enrich_display_meta_from_source(session: &Session, meta: &mut SessionDisplayMeta) -> Result<()> {
    match session.source.as_str() {
        "grok" => enrich_display_meta_from_grok(&session.source_id, meta)?,
        "codex" => {
            if let Some(path) = session.source_file_path.as_deref() {
                enrich_display_meta_from_codex_rollout(Path::new(path), meta)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn enrich_display_meta_from_grok(source_id: &str, meta: &mut SessionDisplayMeta) -> Result<()> {
    let Some(session_dir) = resolve_grok_session_dir(source_id) else {
        return Ok(());
    };
    let summary_path = session_dir.join("summary.json");
    if let Ok(content) = fs::read_to_string(&summary_path)
        && let Ok(doc) = serde_json::from_str::<Value>(&content)
        && let Some(model) = doc.get("current_model_id").and_then(Value::as_str)
    {
        push_unique(&mut meta.models, model);
    }
    let updates_path = session_dir.join("updates.jsonl");
    if !updates_path.exists() {
        return Ok(());
    }
    let file = fs::File::open(&updates_path)
        .with_context(|| format!("failed to open {}", updates_path.display()))?;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if !line.contains("modelId")
            && !line.contains("thinkingDepth")
            && !line.contains("thinking_depth")
            && !line.contains("reasoningEffort")
            && !line.contains("reasoning_effort")
        {
            continue;
        }
        let Ok(doc) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let update = doc.pointer("/params/update").or_else(|| doc.get("update"));
        if let Some(update) = update {
            if let Some(model) = update.pointer("/_meta/modelId").and_then(Value::as_str) {
                push_unique(&mut meta.models, model);
            }
            if let Some(depth) = update.pointer("/_meta/thinkingDepth").and_then(Value::as_str) {
                push_unique(&mut meta.thinking_depths, depth);
            }
            if let Some(depth) = update.pointer("/_meta/thinking_depth").and_then(Value::as_str) {
                push_unique(&mut meta.thinking_depths, depth);
            }
            if let Some(depth) = update.pointer("/_meta/reasoningEffort").and_then(Value::as_str) {
                push_unique(&mut meta.thinking_depths, depth);
            }
            if let Some(depth) = update.pointer("/_meta/reasoning_effort").and_then(Value::as_str) {
                push_unique(&mut meta.thinking_depths, depth);
            }
        }
    }
    Ok(())
}

fn enrich_display_meta_from_codex_rollout(
    path: &Path,
    meta: &mut SessionDisplayMeta,
) -> Result<()> {
    let file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if !line.contains("turn_context") {
            continue;
        }
        let Ok(doc) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if doc.get("type").and_then(Value::as_str) != Some("turn_context") {
            continue;
        }
        let payload = doc.get("payload").unwrap_or(&doc);
        if let Some(model) = payload.get("model").and_then(Value::as_str) {
            push_unique(&mut meta.models, model);
        }
        for key in ["effort", "reasoning_effort", "thinking_depth", "thinkingDepth"] {
            if let Some(depth) = payload.get(key).and_then(Value::as_str) {
                push_unique(&mut meta.thinking_depths, depth);
            }
        }
    }
    Ok(())
}

fn enrich_display_meta_from_json_text(meta: &mut SessionDisplayMeta, raw: &str) {
    let Ok(doc) = serde_json::from_str::<Value>(raw) else {
        return;
    };
    enrich_display_meta_from_json_value(meta, &doc);
}

fn enrich_display_meta_from_json_value(meta: &mut SessionDisplayMeta, doc: &Value) {
    for key in ["model", "model_name", "model_id", "current_model_id"] {
        if let Some(model) = doc.get(key).and_then(Value::as_str) {
            push_unique(&mut meta.models, model);
        }
    }
    for key in ["effort", "reasoning_effort", "thinking_depth", "thinkingDepth", "reasoningEffort"]
    {
        if let Some(depth) = doc.get(key).and_then(Value::as_str) {
            push_unique(&mut meta.thinking_depths, depth);
        }
    }
    if let Some(model_info) = doc.get("model_info") {
        enrich_display_meta_from_json_value(meta, model_info);
    }
}

fn resolve_grok_session_dir(source_id: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let sessions_dir = home.join(".grok").join("sessions");
    let workspaces = fs::read_dir(sessions_dir).ok()?;
    for workspace in workspaces.flatten() {
        let session_dir = workspace.path().join(source_id);
        if session_dir.join("summary.json").exists() {
            return Some(session_dir);
        }
    }
    None
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if trimmed.is_empty() || values.iter().any(|existing| existing == trimmed) {
        return;
    }
    values.push(trimmed.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn collect_display_meta_from_usage_and_codex_rollout() {
        let mut meta = SessionDisplayMeta::default();
        enrich_display_meta_from_usage(
            &mut meta,
            &[SessionUsageEventRecord {
                event_key: "e1".to_string(),
                event_seq: 0,
                message_seq: None,
                timestamp: 0,
                model: "gpt-5.5".to_string(),
                provider: "openai".to_string(),
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
                token_source: "observed".to_string(),
                parser_version: 1,
                source_path: None,
                raw_usage_json: Some(r#"{"effort":"high"}"#.to_string()),
            }],
        );

        let dir = std::env::temp_dir().join(format!("recall-share-meta-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let rollout = dir.join("rollout.jsonl");
        fs::write(
            &rollout,
            r#"{"type":"turn_context","payload":{"model":"gpt-5-codex","effort":"medium"}}"#,
        )
        .unwrap();
        enrich_display_meta_from_codex_rollout(&rollout, &mut meta).unwrap();
        let _ = fs::remove_dir_all(dir);

        assert_eq!(meta.models, vec!["gpt-5.5", "gpt-5-codex"]);
        assert_eq!(meta.thinking_depths, vec!["high", "medium"]);
    }
}
