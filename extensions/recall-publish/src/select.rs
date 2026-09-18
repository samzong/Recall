use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Duration, FixedOffset, NaiveDate, TimeZone, Utc};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct TimeSelection {
    pub field: &'static str,
    pub since: Option<String>,
    pub until: Option<String>,
    pub timezone: String,
}

#[derive(Clone, Debug)]
pub struct TimeWindow {
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub since_local: Option<DateTime<FixedOffset>>,
    pub until_local: Option<DateTime<FixedOffset>>,
    pub timezone: String,
    pub offset: FixedOffset,
}

impl TimeWindow {
    pub fn parse(since: Option<&str>, until: Option<&str>, timezone: &str) -> Result<Self> {
        let offset = parse_timezone(timezone)?;
        let since_local = since.map(|value| parse_boundary(value, offset)).transpose()?;
        let until_local = until.map(|value| parse_boundary(value, offset)).transpose()?;
        if let (Some(since), Some(until)) = (since_local, until_local)
            && until <= since
        {
            bail!("--until must be later than --since");
        }
        Ok(Self {
            since_ms: since_local.map(|value| value.timestamp_millis()),
            until_ms: until_local.map(|value| value.timestamp_millis()),
            since_local,
            until_local,
            timezone: timezone.to_string(),
            offset,
        })
    }

    pub fn contains(&self, started_at: i64) -> bool {
        if let Some(since) = self.since_ms
            && started_at < since
        {
            return false;
        }
        if let Some(until) = self.until_ms
            && started_at >= until
        {
            return false;
        }
        true
    }

    pub fn is_open(&self) -> bool {
        self.since_ms.is_none() && self.until_ms.is_none()
    }

    pub fn selection(&self) -> TimeSelection {
        TimeSelection {
            field: "session.started_at",
            since: self.since_local.map(|value| value.to_rfc3339()),
            until: self.until_local.map(|value| value.to_rfc3339()),
            timezone: self.timezone.clone(),
        }
    }

    pub fn label(&self) -> Option<String> {
        let (since, until) = (self.since_local?, self.until_local?);
        if since.day() == 1
            && until.day() == 1
            && since.time() == until.time()
            && since.time() == chrono::NaiveTime::MIN
            && months_between(since, until) == 1
        {
            return Some(format!("{:04}-{:02}", since.year(), since.month()));
        }
        if since.time() == chrono::NaiveTime::MIN && until == since + Duration::days(1) {
            return Some(format!("{:04}-{:02}-{:02}", since.year(), since.month(), since.day()));
        }
        Some(format!(
            "{:04}{:02}{:02}-{:04}{:02}{:02}",
            since.year(),
            since.month(),
            since.day(),
            until.year(),
            until.month(),
            until.day()
        ))
    }
}

fn months_between(since: DateTime<FixedOffset>, until: DateTime<FixedOffset>) -> i32 {
    (until.year() - since.year()) * 12 + until.month() as i32 - since.month() as i32
}

fn parse_timezone(timezone: &str) -> Result<FixedOffset> {
    if timezone.eq_ignore_ascii_case("UTC") || timezone == "Z" {
        return Ok(FixedOffset::east_opt(0).expect("UTC is a valid offset"));
    }
    let (sign, rest) = match timezone.as_bytes().first() {
        Some(b'+') => (1, &timezone[1..]),
        Some(b'-') => (-1, &timezone[1..]),
        _ => bail!("unsupported timezone `{timezone}`; use UTC or a fixed offset such as +08:00"),
    };
    let (hours, minutes) = rest.split_once(':').context("fixed offsets look like +08:00")?;
    let hours: i32 = hours.parse().context("fixed offsets look like +08:00")?;
    let minutes: i32 = minutes.parse().context("fixed offsets look like +08:00")?;
    if !(0..=23).contains(&hours) || !(0..=59).contains(&minutes) {
        bail!("fixed offset `{timezone}` is out of range");
    }
    FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60))
        .with_context(|| format!("fixed offset `{timezone}` is out of range"))
}

fn parse_boundary(value: &str, offset: FixedOffset) -> Result<DateTime<FixedOffset>> {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Ok(parsed.with_timezone(&offset));
    }
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").with_context(|| {
        format!("`{value}` is not a date (YYYY-MM-DD) or an RFC 3339 timestamp")
    })?;
    offset
        .from_local_datetime(&date.and_time(chrono::NaiveTime::MIN))
        .single()
        .with_context(|| format!("`{value}` is ambiguous in the selected timezone"))
}

pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(ch.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    slug
}

pub fn scope_name(
    author: Option<&str>,
    project: Option<&str>,
    sources: &[String],
    time_label: Option<&str>,
) -> String {
    let mut parts = Vec::new();
    if let Some(author) = author {
        parts.push(format!("author-{}", slugify(author)));
    }
    if let Some(project) = project.filter(|value| *value != "all") {
        parts.push(format!("project-{}", slugify(project)));
    }
    if !sources.is_empty() {
        let mut sources: Vec<String> = sources.iter().map(|value| slugify(value)).collect();
        sources.sort();
        parts.push(format!("agent-{}", sources.join("-")));
    }
    if let Some(label) = time_label {
        parts.push(format!("time-{label}"));
    }
    if parts.is_empty() {
        return "sessions".to_string();
    }
    parts.join("_")
}

pub fn data_filename(scope: &str) -> String {
    format!("{scope}.recall.jsonl")
}

pub fn validate_scope_name(scope: &str) -> Result<()> {
    if scope.is_empty() || scope.len() > 120 {
        bail!("scope name must be between 1 and 120 characters");
    }
    if !scope.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("scope name may only contain letters, digits, dot, dash, and underscore");
    }
    if scope.starts_with('.') || scope.contains("..") {
        bail!("scope name must not start with a dot or contain `..`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(since: &str, until: &str, tz: &str) -> TimeWindow {
        TimeWindow::parse(Some(since), Some(until), tz).unwrap()
    }

    #[test]
    fn selection_interval_is_half_open_on_started_at() {
        let window = window("2026-08-01", "2026-09-01", "UTC");
        let since = window.since_ms.unwrap();
        let until = window.until_ms.unwrap();
        assert!(window.contains(since));
        assert!(window.contains(until - 1));
        assert!(!window.contains(until));
        assert!(!window.contains(since - 1));
    }

    #[test]
    fn bare_dates_resolve_at_midnight_in_the_declared_timezone() {
        let utc = window("2026-08-01", "2026-09-01", "UTC");
        let shanghai = window("2026-08-01", "2026-09-01", "+08:00");
        assert_eq!(utc.since_ms.unwrap() - shanghai.since_ms.unwrap(), 8 * 3600 * 1000);
        assert_eq!(shanghai.selection().timezone, "+08:00");
    }

    #[test]
    fn filename_labels_describe_the_interval_not_the_upload() {
        assert_eq!(window("2026-08-01", "2026-09-01", "UTC").label().unwrap(), "2026-08");
        assert_eq!(window("2026-08-05", "2026-08-06", "UTC").label().unwrap(), "2026-08-05");
        assert_eq!(window("2026-08-05", "2026-09-10", "UTC").label().unwrap(), "20260805-20260910");
        assert!(TimeWindow::parse(Some("2026-08-01"), None, "UTC").unwrap().label().is_none());
    }

    #[test]
    fn scope_components_are_ordered_author_project_agent_time() {
        let scope = scope_name(
            Some("samzong"),
            Some("samzong/Recall"),
            &["codex".to_string()],
            Some("2026-08"),
        );
        assert_eq!(scope, "author-samzong_project-samzong-recall_agent-codex_time-2026-08");
        assert_eq!(
            data_filename(&scope),
            "author-samzong_project-samzong-recall_agent-codex_time-2026-08.recall.jsonl"
        );
    }

    #[test]
    fn global_project_selector_is_not_a_filename_component() {
        assert_eq!(scope_name(None, Some("all"), &[], None), "sessions");
    }

    #[test]
    fn scope_names_cannot_escape_the_workspace() {
        assert!(validate_scope_name("../etc").is_err());
        assert!(validate_scope_name(".hidden").is_err());
        assert!(validate_scope_name("a/b").is_err());
        assert!(validate_scope_name("project-recall_time-2026-08").is_ok());
    }

    #[test]
    fn reversed_intervals_are_rejected() {
        assert!(TimeWindow::parse(Some("2026-09-01"), Some("2026-08-01"), "UTC").is_err());
        assert!(TimeWindow::parse(Some("2026-08-01"), Some("2026-08-01"), "UTC").is_err());
    }
}
