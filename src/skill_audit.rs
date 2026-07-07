use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{Local, TimeZone};
use serde::Serialize;
use serde_json::Value;

use crate::db::search::TimeRange;
use crate::db::store::{SkillAuditEventRow, Store};

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
    let installed = scan_installed_skills();
    let installed_ids: HashSet<String> = installed.iter().map(|skill| skill.id.clone()).collect();
    let events = store.list_skill_audit_events(filters.sources.as_deref(), filters.time_range)?;
    let mut usage: HashMap<String, SkillAccumulator> = HashMap::new();
    for event in &events {
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

    let coverage_note = if events.is_empty() {
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
    for key in ["skill", "name"] {
        if let Some(skill) = value.get(key).and_then(|skill| skill.as_str()) {
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
mod tests {
    use super::*;

    fn installed(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    #[test]
    fn skill_id_from_path_reads_parent_skill_md() {
        assert_eq!(
            skill_id_from_path("/Users/x/.agents/skills/pre-ship/SKILL.md").as_deref(),
            Some("pre-ship")
        );
    }

    #[test]
    fn skill_id_from_path_reads_reference_files() {
        assert_eq!(
            skill_id_from_path("/Users/x/.agents/skills/pre-ship/references/codex-style.md")
                .as_deref(),
            Some("pre-ship")
        );
    }

    #[test]
    fn resolve_skill_id_uses_installed_canonical_name() {
        let installed = installed(&["commit", "pre-ship"]);
        assert_eq!(resolve_skill_id("commit", &installed), "commit");
        assert_eq!(resolve_skill_id("code-gate:source-align", &installed), "code-gate");
    }

    #[test]
    fn extract_skill_from_skill_tool_attrs() {
        let event = SkillAuditEventRow {
            session_id: "s1".to_string(),
            source: "codex".to_string(),
            timestamp: Some(1),
            name: Some("Skill".to_string()),
            target: None,
            attrs_json: Some(r#"{"skill":"commit"}"#.to_string()),
        };
        let (id, signal) = extract_skill_from_event(&event, &installed(&["commit"])).unwrap();
        assert_eq!(id, "commit");
        assert_eq!(signal, SkillSignal::SkillTool);
    }

    #[test]
    fn tiering_uses_core_threshold() {
        let accumulator =
            SkillAccumulator { invocations: CORE_INVOCATION_THRESHOLD, ..Default::default() };
        let entry =
            build_entry("ship".to_string(), "~/.claude/skills/ship".to_string(), accumulator);
        assert_eq!(entry.tier, SkillTier::Core);
    }

    #[test]
    fn skill_id_from_path_reads_skill_md_in_shell_command() {
        assert_eq!(
            skill_id_from_path(r#"cat "/Users/x/.agents/skills/pre-ship/SKILL.md" && echo ok"#)
                .as_deref(),
            Some("pre-ship")
        );
    }

    #[test]
    fn extract_skill_from_opencode_native_skill_tool() {
        let event = SkillAuditEventRow {
            session_id: "s1".to_string(),
            source: "opencode".to_string(),
            timestamp: Some(1),
            name: Some("skill".to_string()),
            target: None,
            attrs_json: Some(r#"{"name":"commit"}"#.to_string()),
        };
        let (id, signal) = extract_skill_from_event(&event, &installed(&["commit"])).unwrap();
        assert_eq!(id, "commit");
        assert_eq!(signal, SkillSignal::SkillTool);
    }

    #[test]
    fn extract_skill_from_opencode_use_skill_plugin() {
        let event = SkillAuditEventRow {
            session_id: "s1".to_string(),
            source: "opencode".to_string(),
            timestamp: Some(1),
            name: Some("use_skill".to_string()),
            target: None,
            attrs_json: Some(r#"{"skill":"pre-ship"}"#.to_string()),
        };
        let (id, signal) = extract_skill_from_event(&event, &installed(&["pre-ship"])).unwrap();
        assert_eq!(id, "pre-ship");
        assert_eq!(signal, SkillSignal::SkillTool);
    }

    #[test]
    fn dedupe_counts_one_invocation_per_session() {
        let mut usage: HashMap<String, SkillAccumulator> = HashMap::new();
        let installed = installed(&["pre-ship"]);
        let events = [
            SkillAuditEventRow {
                session_id: "s1".to_string(),
                source: "codex".to_string(),
                timestamp: Some(1),
                name: Some("Skill".to_string()),
                target: None,
                attrs_json: Some(r#"{"skill":"pre-ship"}"#.to_string()),
            },
            SkillAuditEventRow {
                session_id: "s1".to_string(),
                source: "codex".to_string(),
                timestamp: Some(2),
                name: None,
                target: Some("/Users/x/.agents/skills/pre-ship/references/a.md".to_string()),
                attrs_json: None,
            },
            SkillAuditEventRow {
                session_id: "s2".to_string(),
                source: "codex".to_string(),
                timestamp: Some(3),
                name: Some("Skill".to_string()),
                target: None,
                attrs_json: Some(r#"{"skill":"pre-ship"}"#.to_string()),
            },
        ];
        for event in &events {
            let Some((skill_id, signal)) = extract_skill_from_event(event, &installed) else {
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
        let accumulator = usage.remove("pre-ship").unwrap();
        assert_eq!(accumulator.invocations, 2);
        assert_eq!(accumulator.sessions.len(), 2);
        assert_eq!(accumulator.signals.len(), 2);
        assert_eq!(accumulator.last_used, Some(3));
    }

    #[test]
    fn list_skill_audit_events_includes_command_wrapped_skill_md_reads() {
        use crate::db::schema;
        use crate::db::search::TimeRange;
        use crate::db::store::Store;
        use crate::types::{RawSessionEvent, Session};

        schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let session = Session {
            id: "session-1".to_string(),
            source: "codex".to_string(),
            source_id: "codex-1".to_string(),
            title: "Codex session".to_string(),
            directory: None,
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
            started_at: now,
            updated_at: Some(now),
            message_count: 0,
            entrypoint: None,
            custom_title: None,
            summary: None,
            duration_minutes: None,
            source_file_path: None,
            is_import: false,
        };
        let event = RawSessionEvent {
            event_seq: 0,
            timestamp: Some(now),
            kind: "command".to_string(),
            actor: "assistant".to_string(),
            name: Some("bash".to_string()),
            status: None,
            target: Some(
                r#"cat "/Users/x/.agents/skills/pre-ship/SKILL.md" && echo ok"#.to_string(),
            ),
            message_seq: None,
            summary: None,
            source_path: None,
            source_event_id: None,
            attrs_json: None,
            parser_version: 1,
        };
        store
            .persist_session_with_usage_and_events(&session, &[], &[], None, &[event], Some(1))
            .unwrap();

        let events = store.list_skill_audit_events(None, TimeRange::All).unwrap();
        assert_eq!(events.len(), 1);
        let (id, signal) = extract_skill_from_event(&events[0], &installed(&["pre-ship"])).unwrap();
        assert_eq!(id, "pre-ship");
        assert_eq!(signal, SkillSignal::ReadSkillFile);
    }

    #[test]
    fn list_skill_audit_events_uses_session_timestamp_when_event_timestamp_missing() {
        use crate::db::schema;
        use crate::db::search::TimeRange;
        use crate::db::store::Store;
        use crate::types::{RawSessionEvent, Session};

        schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let session = Session {
            id: "session-1".to_string(),
            source: "cursor".to_string(),
            source_id: "cursor-1".to_string(),
            title: "Cursor session".to_string(),
            directory: None,
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
            started_at: now,
            updated_at: Some(now),
            message_count: 0,
            entrypoint: None,
            custom_title: None,
            summary: None,
            duration_minutes: None,
            source_file_path: None,
            is_import: false,
        };
        let event = RawSessionEvent {
            event_seq: 0,
            timestamp: None,
            kind: "tool_call".to_string(),
            actor: "assistant".to_string(),
            name: Some("Skill".to_string()),
            status: None,
            target: None,
            message_seq: None,
            summary: None,
            source_path: None,
            source_event_id: None,
            attrs_json: Some(r#"{"skill":"commit"}"#.to_string()),
            parser_version: 1,
        };
        store
            .persist_session_with_usage_and_events(&session, &[], &[], None, &[event], Some(1))
            .unwrap();

        assert_eq!(store.list_skill_audit_events(None, TimeRange::All).unwrap().len(), 1);
        let ranged = store.list_skill_audit_events(None, TimeRange::Month).unwrap();
        assert_eq!(ranged.len(), 1);
        assert_eq!(ranged[0].timestamp, Some(now));
    }
}
