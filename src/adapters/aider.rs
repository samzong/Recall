use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use tracing::warn;

use crate::adapters::AdapterSyncContext;
use crate::adapters::file_scan::{self, FileScanEntry, FileScanOptions};
use crate::adapters::{RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult};
use crate::types::Role;

const SOURCE: &str = "aider";
const HISTORY_NAME: &str = ".aider.chat.history.md";

pub(crate) struct AiderAdapter;

impl SourceAdapter for AiderAdapter {
    fn id(&self) -> &str {
        SOURCE
    }

    fn label(&self) -> &str {
        "AID"
    }

    fn resume_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        None
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let mut sessions = Vec::new();
        for entry in collect_history_entries(&discovery_roots()) {
            let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
                continue;
            };
            if let Some(raw) = parse_history_entry(entry, mtime_ms)? {
                sessions.push(raw);
            }
        }
        Ok(sessions)
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        _include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        Ok(Some(file_scan::run_file_scan_with_options(
            context,
            since_ts,
            FileScanOptions::default(),
            collect_history_entries(&discovery_roots()),
            parse_history_entry,
        )?))
    }
}

fn discovery_roots() -> Vec<PathBuf> {
    discovery_roots_from(std::env::current_dir().ok(), dirs::home_dir())
}

fn discovery_roots_from(cwd: Option<PathBuf>, home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    if let Some(cwd) = cwd {
        for dir in cwd_and_git_ancestors(&cwd) {
            push_unique(&mut roots, &mut seen, dir);
        }
    }
    if let Some(git_root) = home.map(|home| home.join("git"))
        && git_root.is_dir()
    {
        for dir in bounded_git_layout_dirs(&git_root) {
            push_unique(&mut roots, &mut seen, dir);
        }
    }
    roots
}

fn cwd_and_git_ancestors(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = cwd.to_path_buf();
    loop {
        dirs.push(current.clone());
        if current.join(".git").exists() {
            break;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => break,
        }
    }
    dirs
}

fn bounded_git_layout_dirs(git_root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![git_root.to_path_buf()];
    let read = match fs::read_dir(git_root) {
        Ok(read) => read,
        Err(_) => return dirs,
    };
    for org in read.flatten() {
        let org_path = org.path();
        if !org_path.is_dir() {
            continue;
        }
        dirs.push(org_path.clone());
        let Ok(projects) = fs::read_dir(&org_path) else {
            continue;
        };
        for project in projects.flatten() {
            let project_path = project.path();
            if project_path.is_dir() {
                dirs.push(project_path);
            }
        }
    }
    dirs
}

fn push_unique(dirs: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, dir: PathBuf) {
    if seen.insert(dir.clone()) {
        dirs.push(dir);
    }
}

fn collect_history_entries(roots: &[PathBuf]) -> Vec<FileScanEntry> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        let path = root.join(HISTORY_NAME);
        if !path.is_file() {
            continue;
        }
        let Ok(canonical) = path.canonicalize() else {
            continue;
        };
        if !seen.insert(canonical.clone()) {
            continue;
        }
        let session_id = root.to_string_lossy().into_owned();
        entries.push(FileScanEntry {
            session_id,
            stat_target: canonical,
            directory: Some(root.to_string_lossy().into_owned()),
        });
    }
    entries
}

fn parse_history_entry(entry: FileScanEntry, mtime_ms: i64) -> anyhow::Result<Option<RawSession>> {
    match parse_history_file(&entry.stat_target, &entry.session_id, entry.directory, mtime_ms) {
        Ok(raw) => Ok(raw),
        Err(err) => {
            warn!("failed to parse Aider history {}: {err}", entry.stat_target.display());
            Ok(None)
        }
    }
}

fn parse_history_file(
    path: &Path,
    source_id: &str,
    directory: Option<String>,
    mtime_ms: i64,
) -> anyhow::Result<Option<RawSession>> {
    let text = fs::read_to_string(path)?;
    parse_history_markdown(&text, source_id, directory, mtime_ms, path.to_str().map(str::to_string))
}

fn parse_history_markdown(
    text: &str,
    source_id: &str,
    directory: Option<String>,
    mtime_ms: i64,
    source_path: Option<String>,
) -> anyhow::Result<Option<RawSession>> {
    let messages = split_chat_history_markdown(text);
    if messages.is_empty() {
        return Ok(None);
    }
    let started_at = session_start_ms(text).unwrap_or(mtime_ms);
    let mut raw = RawSession::search_only(
        source_id.to_string(),
        directory,
        started_at,
        Some(mtime_ms),
        None,
        messages,
    );
    raw.source_file_path = source_path;
    Ok(Some(raw))
}

fn split_chat_history_markdown(text: &str) -> Vec<RawMessage> {
    let mut messages = Vec::new();
    let mut user = Vec::new();
    let mut assistant = Vec::new();
    let mut tool = Vec::new();

    fn flush(role: Role, lines: &mut Vec<String>, messages: &mut Vec<RawMessage>) {
        let content = lines.join("").trim().to_string();
        lines.clear();
        if !content.is_empty() {
            messages.push(RawMessage { role, content, timestamp: None });
        }
    }

    for line in text.split_inclusive('\n') {
        if line.starts_with("# ") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("> ") {
            flush(Role::Assistant, &mut assistant, &mut messages);
            flush(Role::User, &mut user, &mut messages);
            tool.push(rest.to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("#### ") {
            flush(Role::Assistant, &mut assistant, &mut messages);
            tool.clear();
            user.push(rest.to_string());
            continue;
        }
        flush(Role::User, &mut user, &mut messages);
        tool.clear();
        assistant.push(line.to_string());
    }
    flush(Role::Assistant, &mut assistant, &mut messages);
    flush(Role::User, &mut user, &mut messages);
    messages
}

fn session_start_ms(text: &str) -> Option<i64> {
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("# aider chat started at ") else {
            continue;
        };
        let rest = rest.trim();
        if rest.is_empty() {
            continue;
        }
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(rest, "%Y-%m-%d %H:%M:%S") {
            return Some(naive.and_utc().timestamp_millis());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/aider")
    }

    #[test]
    fn resume_is_none() {
        assert!(AiderAdapter.resume_command("/tmp/proj").is_none());
    }

    #[test]
    fn official_format_uses_heading_for_user_and_skips_quotes() {
        let text =
            fs::read_to_string(fixtures_dir().join("official.aider.chat.history.md")).unwrap();
        let session =
            parse_history_markdown(&text, "/tmp/proj", Some("/tmp/proj".to_string()), 1, None)
                .unwrap()
                .unwrap();
        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(
            session.messages[0].content,
            "Show me the failing query\nand the tenant predicate"
        );
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].content, "The query misses the tenant predicate.");
        assert_eq!(session.messages[2].role, Role::User);
        assert_eq!(session.messages[2].content, "also check tests");
        assert_eq!(session.messages[3].role, Role::Assistant);
        assert_eq!(session.messages[3].content, "Tests look fine.");
        assert!(session.messages.iter().all(|message| !message.content.contains("Applied edit")));
    }

    #[test]
    fn fad_style_blockquote_is_not_user() {
        let text = fs::read_to_string(fixtures_dir().join("fad-blockquote-not-user.md")).unwrap();
        let session = parse_history_markdown(&text, "/tmp/proj", None, 1, None).unwrap().unwrap();
        assert!(session.messages.iter().all(|message| message.role != Role::User));
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].role, Role::Assistant);
        assert_eq!(session.messages[0].content, "The assistant reply stays assistant.");
        assert!(!session.messages.iter().any(|message| {
            message.content.contains("FAD-style") || message.role == Role::User
        }));
    }

    #[test]
    fn discovery_uses_cwd_git_ancestors_and_home_git_only() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("work/repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(repo.join(HISTORY_NAME), "#### hi\n\nok\n").unwrap();
        fs::create_dir_all(home.path().join("git/samzong/Recall")).unwrap();
        fs::write(home.path().join("git/samzong/Recall").join(HISTORY_NAME), "#### git layout\n")
            .unwrap();
        fs::create_dir_all(home.path().join("secret")).unwrap();
        fs::write(home.path().join("secret").join(HISTORY_NAME), "#### leaked\n").unwrap();
        fs::write(home.path().join(HISTORY_NAME), "#### home file\n").unwrap();

        let roots = discovery_roots_from(Some(repo.clone()), Some(home.path().to_path_buf()));
        let entries = collect_history_entries(&roots);
        let ids: Vec<_> = entries.iter().map(|entry| entry.directory.clone()).collect();
        assert!(ids.iter().any(|dir| dir.as_deref() == Some(repo.to_str().unwrap())));
        assert!(ids.iter().any(|dir| {
            dir.as_deref() == Some(home.path().join("git/samzong/Recall").to_str().unwrap())
        }));
        assert!(ids.iter().all(|dir| {
            dir.as_deref() != Some(home.path().to_str().unwrap())
                && dir.as_deref() != Some(home.path().join("secret").to_str().unwrap())
        }));
    }

    #[test]
    fn missing_home_git_is_not_walked() {
        let home = tempfile::tempdir().unwrap();
        let cwd = home.path().join("cwd");
        fs::create_dir_all(&cwd).unwrap();
        let roots = discovery_roots_from(Some(cwd), Some(home.path().to_path_buf()));
        assert!(collect_history_entries(&roots).is_empty());
    }
}
