use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{Local, TimeZone};
use serde::Serialize;
use serde_json::Value;

use crate::db::search::TimeRange;
use crate::db::store::{SkillAuditEventRow, Store};
use crate::project_scope::ProjectScope;
use crate::usage::{UsageTarget, session_partition_key};

pub(crate) const CORE_INVOCATION_THRESHOLD: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum SkillTier {
    Core,
    Occasional,
    Dormant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub(crate) enum SkillSignal {
    ReadSkillFile,
    SkillTool,
}

#[derive(Debug, Clone)]
pub(crate) struct SkillAuditFilters {
    pub(crate) sources: Option<Vec<String>>,
    pub(crate) time_range: TimeRange,
    pub(crate) target: UsageTarget,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SkillAuditSummary {
    pub(crate) installed: usize,
    pub(crate) core: usize,
    pub(crate) occasional: usize,
    pub(crate) dormant: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SkillUsageEntry {
    pub(crate) id: String,
    pub(crate) tier: SkillTier,
    pub(crate) invocations: usize,
    pub(crate) last_used: Option<i64>,
    pub(crate) signals: Vec<SkillSignal>,
    pub(crate) install_path: Option<String>,
    pub(crate) session_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SkillAuditReport {
    pub(crate) summary: SkillAuditSummary,
    pub(crate) core: Vec<SkillUsageEntry>,
    pub(crate) occasional: Vec<SkillUsageEntry>,
    pub(crate) dormant: Vec<SkillUsageEntry>,
    pub(crate) coverage_note: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct InstalledSkill {
    pub(crate) id: String,
    pub(crate) install_path: String,
}

#[derive(Default)]
struct SkillAccumulator {
    invocations: usize,
    sessions: HashSet<String>,
    last_used: Option<i64>,
    signals: BTreeSet<SkillSignal>,
}

pub(crate) fn build_skill_audit_report(
    store: &Store,
    filters: &SkillAuditFilters,
) -> Result<SkillAuditReport> {
    build_skill_audit_report_with_installed(store, filters, scan_installed_skills())
}

fn build_skill_audit_report_with_installed(
    store: &Store,
    filters: &SkillAuditFilters,
    installed: Vec<InstalledSkill>,
) -> Result<SkillAuditReport> {
    let installed_ids: HashSet<String> = installed.iter().map(|skill| skill.id.clone()).collect();
    let scope_ids = match &filters.target {
        UsageTarget::Scope(ProjectScope::Global) => None,
        UsageTarget::Scope(scope) => Some(store.session_ids_in_scope(scope)?),
        UsageTarget::Partition { .. } => None,
    };
    let session_meta = match &filters.target {
        UsageTarget::Partition { .. } => {
            Some(store.session_usage_meta_map(filters.sources.as_deref())?)
        }
        UsageTarget::Scope(_) => None,
    };
    let in_target = |session_id: &str| -> bool {
        match &filters.target {
            UsageTarget::Scope(ProjectScope::Global) => true,
            UsageTarget::Scope(_) => scope_ids.as_ref().is_some_and(|ids| ids.contains(session_id)),
            UsageTarget::Partition { key, .. } => {
                session_partition_key(session_meta.as_ref().and_then(|map| map.get(session_id)))
                    == *key
            }
        }
    };

    let mut usage: HashMap<String, SkillAccumulator> = HashMap::new();
    let has_any_events = if store.skill_usage_index_ready()? {
        let cutoff = filters.time_range.millis_ago();
        let rows = store.list_indexed_skill_usage(filters.sources.as_deref())?;
        let mut any_events = false;
        for row in rows {
            let t_nonnull = row.max_timestamp.filter(|t| cutoff.is_none_or(|c| *t >= c));
            let t_null = if row.has_null_timestamp && cutoff.is_none_or(|c| row.session_ts >= c) {
                Some(row.session_ts)
            } else {
                None
            };
            let effective_ts = match (t_nonnull, t_null) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            let Some(timestamp) = effective_ts else {
                continue;
            };
            any_events = true;
            if !in_target(&row.session_id) {
                continue;
            }
            let signal = match row.signal.as_str() {
                "skill_tool" => SkillSignal::SkillTool,
                "read_file" => SkillSignal::ReadSkillFile,
                _ => continue,
            };
            let skill_id = resolve_skill_id(&row.raw_skill, &installed_ids);
            let entry = usage.entry(skill_id).or_default();
            if entry.sessions.insert(row.session_id) {
                entry.invocations += 1;
            }
            entry.signals.insert(signal);
            entry.last_used = Some(entry.last_used.map_or(timestamp, |cur| cur.max(timestamp)));
        }
        any_events
    } else {
        let events =
            store.list_skill_audit_events(filters.sources.as_deref(), filters.time_range)?;
        let any_events = !events.is_empty();
        for event in &events {
            if !in_target(&event.session_id) {
                continue;
            }
            let Some((skill_id, signal)) = extract_skill_from_event(event, &installed_ids) else {
                continue;
            };
            let entry = usage.entry(skill_id).or_default();
            let first_in_session = entry.sessions.insert(event.session_id.clone());
            if first_in_session {
                entry.invocations += 1;
            }
            entry.signals.insert(signal);
            if let Some(timestamp) = event.timestamp {
                entry.last_used =
                    Some(entry.last_used.map_or(timestamp, |current| current.max(timestamp)));
            }
        }
        any_events
    };

    let mut core = Vec::new();
    let mut occasional = Vec::new();
    let mut dormant = Vec::new();

    for skill in installed {
        let accumulator = usage.remove(&skill.id).unwrap_or_default();
        let entry = build_entry(skill.id, skill.install_path, accumulator);
        match entry.tier {
            SkillTier::Core => core.push(entry),
            SkillTier::Occasional => occasional.push(entry),
            SkillTier::Dormant => dormant.push(entry),
        }
    }

    sort_entries(&mut core);
    sort_entries(&mut occasional);
    dormant.sort_by(|left, right| left.id.cmp(&right.id));

    let summary = SkillAuditSummary {
        installed: core.len() + occasional.len() + dormant.len(),
        core: core.len(),
        occasional: occasional.len(),
        dormant: dormant.len(),
    };

    let coverage_note = if !has_any_events {
        Some(
            "No skill activity in index. Run `recall sync --force` on codex/claude/cursor sources."
                .to_string(),
        )
    } else {
        None
    };

    Ok(SkillAuditReport { summary, core, occasional, dormant, coverage_note })
}

fn build_entry(id: String, install_path: String, accumulator: SkillAccumulator) -> SkillUsageEntry {
    let tier = if accumulator.invocations >= CORE_INVOCATION_THRESHOLD {
        SkillTier::Core
    } else if accumulator.invocations > 0 {
        SkillTier::Occasional
    } else {
        SkillTier::Dormant
    };
    let mut session_ids: Vec<String> = accumulator.sessions.into_iter().collect();
    session_ids.sort();
    SkillUsageEntry {
        id,
        tier,
        invocations: accumulator.invocations,
        last_used: accumulator.last_used,
        signals: accumulator.signals.into_iter().collect(),
        install_path: Some(install_path),
        session_ids,
    }
}

fn sort_entries(entries: &mut [SkillUsageEntry]) {
    entries.sort_by(|left, right| {
        right.invocations.cmp(&left.invocations).then_with(|| left.id.cmp(&right.id))
    });
}

pub(crate) fn scan_installed_skills() -> Vec<InstalledSkill> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let mut by_id: HashMap<String, InstalledSkill> = HashMap::new();
    for root in personal_skill_roots(&home) {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.join("SKILL.md").is_file() {
                continue;
            }
            let Some(id) = path.file_name().and_then(|name| name.to_str()).map(str::to_string)
            else {
                continue;
            };
            by_id
                .entry(id.clone())
                .or_insert(InstalledSkill { id, install_path: shorten_home_path(&path) });
        }
    }
    let mut skills: Vec<_> = by_id.into_values().collect();
    skills.sort_by(|left, right| left.id.cmp(&right.id));
    skills
}

fn personal_skill_roots(home: &Path) -> Vec<PathBuf> {
    [home.join(".claude/skills"), home.join(".codex/skills"), home.join(".agents/skills")]
        .into_iter()
        .filter(|path| path.is_dir())
        .collect()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn shorten_home_path(path: &Path) -> String {
    if let Some(home) = home_dir()
        && let Ok(stripped) = path.strip_prefix(&home)
    {
        return format!("~/{}", stripped.display());
    }
    path.display().to_string()
}

pub(crate) fn classify_skill_event(
    kind: &str,
    name: Option<&str>,
    target: Option<&str>,
    attrs_json: Option<&str>,
) -> Option<(String, &'static str)> {
    if kind == "tool_result" {
        return None;
    }
    let is_tool = name.is_some_and(is_skill_tool_name);
    let is_target_candidate = target.is_some_and(is_skill_target_candidate);
    if !is_tool && !is_target_candidate {
        return None;
    }
    if is_tool && let Some(raw) = skill_from_attrs(attrs_json) {
        return Some((raw, "skill_tool"));
    }
    if let Some(target) = target
        && let Some(raw) = skill_id_from_path(target)
    {
        return Some((raw, "read_file"));
    }
    Some((String::new(), "candidate_only"))
}

fn is_skill_target_candidate(target: &str) -> bool {
    let lower = target.to_ascii_lowercase();
    let mut rest = lower.as_str();
    while let Some(idx) = rest.find("/skills/") {
        let after = &rest[idx + "/skills/".len()..];
        if after.contains("/skill.md") || after.contains("/references/") {
            return true;
        }
        rest = after;
    }
    false
}

fn extract_skill_from_event(
    event: &SkillAuditEventRow,
    installed_ids: &HashSet<String>,
) -> Option<(String, SkillSignal)> {
    if event.name.as_deref().is_some_and(is_skill_tool_name)
        && let Some(raw) = skill_from_attrs(event.attrs_json.as_deref())
    {
        return Some((resolve_skill_id(&raw, installed_ids), SkillSignal::SkillTool));
    }
    if let Some(target) = event.target.as_deref()
        && let Some(raw) = skill_id_from_path(target)
    {
        return Some((resolve_skill_id(&raw, installed_ids), SkillSignal::ReadSkillFile));
    }
    None
}

fn is_skill_tool_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("skill") || name.eq_ignore_ascii_case("use_skill")
}

fn skill_from_attrs(attrs_json: Option<&str>) -> Option<String> {
    let attrs = attrs_json?;
    let value: Value = serde_json::from_str(attrs).ok()?;
    let value = value.get("part").unwrap_or(&value);
    let args = match value.get("type").and_then(Value::as_str) {
        Some("tool") => value.pointer("/state/input")?,
        Some("tool-invocation") => value.get("input")?,
        Some("tool_use") => value.get("input")?,
        Some("tool-call") => value.get("args")?,
        Some("function_call" | "custom_tool_call") => {
            value.get("arguments").or_else(|| value.get("input"))?
        }
        Some(
            "tool_result" | "tool-result" | "function_call_output" | "custom_tool_call_output",
        ) => return None,
        _ => value,
    };
    let decoded = args.as_str().and_then(|text| serde_json::from_str::<Value>(text).ok());
    let args = decoded.as_ref().unwrap_or(args);
    for key in ["skill", "name"] {
        if let Some(skill) = args.get(key).and_then(|skill| skill.as_str()) {
            return Some(skill.to_string());
        }
    }
    None
}

fn skill_id_from_path(path: &str) -> Option<String> {
    let marker = "/skills/";
    let idx = path.find(marker)?;
    let rest = &path[idx + marker.len()..];
    let mut parts = rest.split('/');
    let skill_id = trim_path_token(parts.next()?);
    if skill_id.is_empty() {
        return None;
    }
    let tail = parts.map(trim_path_token).collect::<Vec<_>>().join("/");
    if tail.is_empty()
        || tail.starts_with("SKILL.md")
        || tail == "references"
        || tail.starts_with("references/")
    {
        return Some(skill_id.to_string());
    }
    None
}

fn trim_path_token(token: &str) -> &str {
    token.trim().trim_matches('"').trim_matches('\'')
}

fn resolve_skill_id(raw: &str, installed_ids: &HashSet<String>) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if installed_ids.contains(trimmed) {
        return trimmed.to_string();
    }
    if let Some((base, _)) = trimmed.split_once(':')
        && installed_ids.contains(base)
    {
        return base.to_string();
    }
    for installed in installed_ids {
        if installed.eq_ignore_ascii_case(trimmed) {
            return installed.clone();
        }
        if let Some((base, _)) = trimmed.split_once(':')
            && installed.eq_ignore_ascii_case(base)
        {
            return installed.clone();
        }
    }
    trimmed
        .split(':')
        .next()
        .unwrap_or(trimmed)
        .split_whitespace()
        .next()
        .unwrap_or(trimmed)
        .to_string()
}

pub(crate) fn format_last_used(timestamp_ms: Option<i64>) -> String {
    let Some(timestamp_ms) = timestamp_ms else {
        return "never".to_string();
    };
    let Some(dt) = Local.timestamp_millis_opt(timestamp_ms).single() else {
        return "unknown".to_string();
    };
    let now = Local::now();
    let days = now.date_naive().signed_duration_since(dt.date_naive()).num_days();
    match days {
        0 => "today".to_string(),
        1 => "1d ago".to_string(),
        2..=6 => format!("{days}d ago"),
        7..=29 => format!("{}w ago", days / 7),
        _ => dt.format("%Y-%m-%d").to_string(),
    }
}

pub(crate) fn format_signals(signals: &[SkillSignal]) -> &'static str {
    let has_tool = signals.contains(&SkillSignal::SkillTool);
    let has_read = signals.contains(&SkillSignal::ReadSkillFile);
    match (has_tool, has_read) {
        (true, true) => "both",
        (true, false) => "invoke",
        (false, true) => "read",
        (false, false) => "-",
    }
}

#[cfg(test)]
mod tests;
