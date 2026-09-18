use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::card;
use crate::config::Config;
use crate::manifest::{
    DATASET_SCHEMA_NAME, DATASET_SCHEMA_VERSION, MANIFEST_SCHEMA_NAME, MANIFEST_SCHEMA_VERSION,
    PROTOCOL_VERSION, SOURCE_SCHEMA_VERSION,
};
use crate::progress::Progress;
use crate::protocol::RecallClient;
use crate::record::{ExportRecord, parse_export_record, to_public_record, visit_text_leaves};
use crate::scan::{self, Leaf};
use crate::select::{TimeWindow, now_rfc3339};
use crate::workspace::{
    APPROVAL_FILE, Approval, CARD_FILE, FINDINGS_FILE, MANIFEST_FILE, PREVIEW_FILE, Workspace,
    digest_of,
};

pub struct Selection {
    pub project: String,
    pub sources: Vec<String>,
    pub thread_roles: Vec<String>,
    pub window: TimeWindow,
    pub include_ids: Vec<String>,
    pub exclude_ids: Vec<String>,
}

pub struct PrepareRequest<'a> {
    pub selection: Selection,
    pub scope: String,
    pub license: String,
    pub author: String,
    pub config: &'a Config,
    pub env_dir: &'a Path,
    pub progress: &'a Progress,
}

pub fn fetch_records(
    client: &RecallClient,
    selection: &Selection,
    progress: &Progress,
) -> Result<Vec<ExportRecord>> {
    let mut records: BTreeMap<String, ExportRecord> = BTreeMap::new();
    let mut ingest = |stdout: String| -> Result<()> {
        for (index, line) in stdout.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let record = parse_export_record(line, index + 1)?;
            records.insert(record.session_id.clone(), record);
        }
        Ok(())
    };

    progress.step(&format!("Exporting sessions for project `{}`", selection.project));
    if selection.sources.is_empty() {
        ingest(client.export_scope(&selection.project, None)?)?;
    } else {
        for source in &selection.sources {
            progress.step(&format!("Exporting sessions from `{source}`"));
            ingest(client.export_scope(&selection.project, Some(source))?)?;
        }
    }
    if !selection.include_ids.is_empty() {
        ingest(client.export_ids(&selection.include_ids)?)?;
    }
    progress.step(&format!("Recall returned {} session(s)", records.len()));

    let explicit: BTreeSet<&String> = selection.include_ids.iter().collect();
    let excluded: BTreeSet<&String> = selection.exclude_ids.iter().collect();
    let mut selected: Vec<ExportRecord> = records
        .into_values()
        .filter(|record| !excluded.contains(&record.session_id))
        .filter(|record| {
            selection.thread_roles.is_empty()
                || selection.thread_roles.contains(&thread_role(record))
        })
        .filter(|record| {
            explicit.contains(&record.session_id) || selection.window.contains(record.started_at)
        })
        .collect();
    selected.sort_by(|a, b| {
        a.started_at.cmp(&b.started_at).then_with(|| a.session_id.cmp(&b.session_id))
    });
    Ok(selected)
}

pub fn thread_role(record: &ExportRecord) -> String {
    record.value["session"]["topology"]["thread_role"].as_str().unwrap_or("primary").to_string()
}

struct Prepared {
    entities: BTreeMap<String, usize>,
    sessions_redacted: usize,
    findings: Value,
}

fn collect_leaves(records: &[Value]) -> Result<Vec<Leaf>> {
    let mut leaves = Vec::new();
    for (index, record) in records.iter().enumerate() {
        let mut clone = record.clone();
        visit_text_leaves(&mut clone, &mut |pointer, text| {
            if !text.is_empty() {
                leaves.push(Leaf {
                    pointer: pointer.to_string(),
                    record: index,
                    text: text.to_string(),
                });
            }
            None
        })?;
    }
    Ok(leaves)
}

fn redact(
    records: &mut [Value],
    config: &Config,
    env_dir: &Path,
    progress: &Progress,
    allow_unresolved: bool,
) -> Result<Prepared> {
    for record in records.iter_mut() {
        visit_text_leaves(record, &mut |_, text| {
            let substituted = config.apply_substitutions(text);
            (substituted != text).then_some(substituted)
        })?;
    }

    let leaves = collect_leaves(records)?;
    let outcome = scan::scan(env_dir, &leaves, config, progress)?;

    if !outcome.unresolved.is_empty() && !allow_unresolved {
        let first = &outcome.unresolved[0];
        bail!(
            "{} unresolved finding(s) block publication; first: {} at {}",
            outcome.unresolved.len(),
            first.reason,
            if first.pointer.is_empty() { "the scan document" } else { first.pointer.as_str() }
        );
    }

    let mut replacements: BTreeMap<(usize, String), String> = BTreeMap::new();
    let mut fields: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
    let mut per_record_entities: BTreeMap<usize, BTreeMap<String, usize>> = BTreeMap::new();
    let mut per_record_spans: BTreeMap<usize, usize> = BTreeMap::new();

    for (index, redaction) in &outcome.redactions {
        let leaf = &leaves[*index];
        replacements.insert((leaf.record, leaf.pointer.clone()), redaction.text.clone());
        fields.entry(leaf.record).or_default().insert(leaf.pointer.clone());
        *per_record_spans.entry(leaf.record).or_default() += redaction.spans.len();
        let entities = per_record_entities.entry(leaf.record).or_default();
        for span in &redaction.spans {
            *entities.entry(span.entity.clone()).or_default() += 1;
        }
    }

    let mut sessions_redacted = 0usize;
    for (index, record) in records.iter_mut().enumerate() {
        if replacements.keys().any(|(record_index, _)| *record_index == index) {
            visit_text_leaves(record, &mut |pointer, _| {
                replacements.get(&(index, pointer.to_string())).cloned()
            })?;
        }
        let spans = per_record_spans.get(&index).copied().unwrap_or(0);
        let entities = per_record_entities.remove(&index).unwrap_or_default();
        let field_list: Vec<Value> =
            fields.remove(&index).unwrap_or_default().into_iter().map(Value::String).collect();
        if spans > 0 {
            sessions_redacted += 1;
        }
        record["redaction"] = json!({
            "status": if spans > 0 { "redacted" } else { "clean" },
            "spans": spans,
            "entities": entities,
            "fields": field_list,
        });
    }

    Ok(Prepared {
        entities: outcome.entities,
        sessions_redacted,
        findings: json!({
            "findings": outcome.findings,
            "unresolved": outcome.unresolved,
        }),
    })
}

fn serialize(records: &[Value]) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for record in records {
        bytes.extend_from_slice(&serde_json::to_vec(record)?);
        bytes.push(b'\n');
    }
    Ok(bytes)
}

pub fn prepare(
    client: &RecallClient,
    workspace: &Workspace,
    request: &PrepareRequest<'_>,
) -> Result<Value> {
    let progress = request.progress;
    let exported = fetch_records(client, &request.selection, progress)?;
    if exported.is_empty() {
        bail!("the selection matched no sessions");
    }
    progress.step(&format!("{} session(s) match the selection", exported.len()));
    let sources: BTreeSet<String> = exported.iter().map(|record| record.source.clone()).collect();
    let session_ids: Vec<String> =
        exported.iter().map(|record| record.session_id.clone()).collect();

    progress.step("Removing local filesystem locations");
    let mut records: Vec<Value> = exported.iter().map(to_public_record).collect::<Result<_>>()?;

    let prepared = redact(&mut records, request.config, request.env_dir, progress, false)?;
    let bytes = serialize(&records)?;

    progress.step("Rescanning the redacted records");
    let rescan_leaves = collect_leaves(&records)?;
    let (rescan_findings, rescan_unresolved) = scan::run_gitleaks(&rescan_leaves, request.config)?;
    if !rescan_findings.is_empty() || !rescan_unresolved.is_empty() {
        bail!(
            "the rescan still reports {} finding(s); publication is blocked",
            rescan_findings.len() + rescan_unresolved.len()
        );
    }

    progress.step("Writing the workspace");
    workspace.reset()?;
    let data_name = format!("{}{}", workspace.scope, crate::workspace::DATA_FILE_SUFFIX);
    workspace.write(&data_name, &bytes)?;

    let info = client.info().unwrap_or_else(|_| json!({}));
    let manifest = json!({
        "schema": { "name": MANIFEST_SCHEMA_NAME, "version": MANIFEST_SCHEMA_VERSION },
        "dataset_schema": { "name": DATASET_SCHEMA_NAME, "version": DATASET_SCHEMA_VERSION },
        "publisher": { "author": request.author },
        "license": request.license,
        "selection": {
            "project": request.selection.project,
            "sources": request.selection.sources,
            "thread_roles": request.selection.thread_roles,
            "time": request.selection.window.selection(),
            "session_ids": {
                "include": request.selection.include_ids,
                "exclude": request.selection.exclude_ids,
            },
        },
        "files": [{
            "path": data_name,
            "sha256": digest_of(&bytes),
            "bytes": bytes.len(),
            "sessions": records.len(),
        }],
        "redaction": {
            "sessions_redacted": prepared.sessions_redacted,
            "entities": prepared.entities,
            "languages": request.config.languages,
            "tools": environment_versions(request.env_dir).unwrap_or_else(|_| json!({})),
            "coverage_notes": coverage_notes(request.config),
        },
        "producer": {
            "recall_publish": env!("CARGO_PKG_VERSION"),
            "protocol_version": info.get("protocol_version").cloned().unwrap_or(json!(PROTOCOL_VERSION)),
            "source_schema_version": SOURCE_SCHEMA_VERSION,
        },
        "prepared_at": now_rfc3339(),
    });
    workspace.write_json(MANIFEST_FILE, &manifest)?;
    workspace.write(CARD_FILE, card::render(&workspace.scope, &data_name, &manifest).as_bytes())?;

    let preview = json!({
        "scope": workspace.scope,
        "sessions": records.len(),
        "sources": sources,
        "session_ids": session_ids,
        "bytes": bytes.len(),
        "sessions_redacted": prepared.sessions_redacted,
        "entities": prepared.entities,
        "files": workspace.digests()?,
    });
    workspace.write_json(PREVIEW_FILE, &preview)?;
    workspace.write_json(FINDINGS_FILE, &prepared.findings)?;
    workspace.clear_approval()?;
    progress.step("Prepared");
    Ok(preview)
}

fn coverage_notes(config: &Config) -> Vec<String> {
    let mut notes = vec![
        "A Chinese street address is detected only down to the city.".to_string(),
        "Internal decisions and customer context can leak without matching any pattern."
            .to_string(),
    ];
    if !config.allow.presidio_entities.iter().any(|entity| entity == "PERSON") {
        notes.push(
            "PERSON detection is context-dependent; a name inside a code literal can be missed."
                .to_string(),
        );
    }
    if !config.allow.gitleaks_rules.is_empty() {
        notes.push(format!(
            "Publisher disabled gitleaks rules: {}.",
            config.allow.gitleaks_rules.join(", ")
        ));
    }
    if !config.allow.presidio_entities.is_empty() {
        notes.push(format!(
            "Entity types not redacted: {}.",
            config.allow.presidio_entities.join(", ")
        ));
    }
    if !config.allow.identities.is_empty() {
        notes.push(format!(
            "Publisher declared {} identity value(s) public.",
            config.allow.identities.len()
        ));
    }
    notes
}

pub fn environment_versions(env_dir: &Path) -> Result<Value> {
    let output = std::process::Command::new("uv")
        .arg("run")
        .arg("--project")
        .arg(env_dir)
        .arg("--quiet")
        .arg("python")
        .arg("-c")
        .arg(
            "import json,importlib.metadata as m;print(json.dumps({n:m.version(n) for n in ['presidio-analyzer','presidio-anonymizer','spacy','huggingface-hub','en-core-web-lg','zh-core-web-lg']}))",
        )
        .output()
        .context("failed to read the redaction environment versions")?;
    if !output.status.success() {
        bail!("the redaction environment is not ready; run `recall publish doctor --install`");
    }
    let mut versions: Value = serde_json::from_slice(&output.stdout)
        .context("the redaction environment reported unreadable versions")?;
    if let Ok(gitleaks) = std::process::Command::new("gitleaks").arg("version").output()
        && gitleaks.status.success()
    {
        versions["gitleaks"] =
            Value::String(String::from_utf8_lossy(&gitleaks.stdout).trim().to_string());
    }
    Ok(versions)
}

pub fn approve(workspace: &Workspace) -> Result<Approval> {
    if !workspace.exists() {
        bail!("scope `{}` has not been prepared; run `recall publish prepare`", workspace.scope);
    }
    let findings: Value = serde_json::from_slice(&std::fs::read(workspace.path(FINDINGS_FILE))?)
        .context("the local findings record is unreadable; run `recall publish prepare`")?;
    let unresolved =
        findings.get("unresolved").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
    if unresolved > 0 {
        bail!("{unresolved} unresolved finding(s) block approval");
    }
    let approval = Approval {
        scope: workspace.scope.clone(),
        approved_at: now_rfc3339(),
        digests: workspace.digests()?,
    };
    workspace.write_json(APPROVAL_FILE, &serde_json::to_value(&approval)?)?;
    Ok(approval)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::select::TimeWindow;

    fn record(id: &str, started_at: i64, source: &str) -> ExportRecord {
        ExportRecord {
            session_id: id.to_string(),
            started_at,
            source: source.to_string(),
            value: json!({}),
        }
    }

    fn selection(since: Option<&str>, until: Option<&str>) -> Selection {
        Selection {
            project: "all".to_string(),
            sources: Vec::new(),
            thread_roles: Vec::new(),
            window: TimeWindow::parse(since, until, "UTC").unwrap(),
            include_ids: Vec::new(),
            exclude_ids: Vec::new(),
        }
    }

    fn filter(records: Vec<ExportRecord>, selection: &Selection) -> Vec<String> {
        let explicit: BTreeSet<&String> = selection.include_ids.iter().collect();
        let excluded: BTreeSet<&String> = selection.exclude_ids.iter().collect();
        let mut kept: Vec<ExportRecord> = records
            .into_iter()
            .filter(|record| !excluded.contains(&record.session_id))
            .filter(|record| {
                explicit.contains(&record.session_id)
                    || selection.window.contains(record.started_at)
            })
            .collect();
        kept.sort_by_key(|a| a.started_at);
        kept.into_iter().map(|record| record.session_id).collect()
    }

    #[test]
    fn explicit_inclusion_survives_the_time_window() {
        let mut selection = selection(Some("2026-08-01"), Some("2026-09-01"));
        selection.include_ids = vec!["old".to_string()];
        let kept = filter(
            vec![record("old", 0, "codex"), record("inside", 1786752000000, "codex")],
            &selection,
        );
        assert_eq!(kept, vec!["old".to_string(), "inside".to_string()]);
    }

    #[test]
    fn exclusion_is_applied_after_every_other_selector() {
        let mut selection = selection(None, None);
        selection.include_ids = vec!["a".to_string()];
        selection.exclude_ids = vec!["a".to_string()];
        let kept = filter(vec![record("a", 1, "codex"), record("b", 2, "codex")], &selection);
        assert_eq!(kept, vec!["b".to_string()]);
    }

    #[test]
    fn coverage_notes_disclose_publisher_weakened_detection() {
        let mut config = Config::default();
        config.allow.gitleaks_rules = vec!["generic-api-key".to_string()];
        let notes = coverage_notes(&config);
        assert!(notes.iter().any(|note| note.contains("generic-api-key")));
    }

    #[test]
    fn named_entities_are_disclosed_as_unredacted_by_default() {
        let notes = coverage_notes(&Config::default());
        assert!(notes.iter().any(|note| note.contains("PERSON") && note.contains("LOCATION")));
    }
}
