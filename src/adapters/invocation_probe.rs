use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::time::{Duration, SystemTime};

use crate::adapters::file_scan::FileScanEntry;

const MAX_FILES: usize = 64;
const MAX_FILE_BYTES: usize = 256 * 1024;
const MAX_TOTAL_BYTES: usize = 4 * 1024 * 1024;
const RECENT_WINDOW: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InvocationProbeCandidate {
    pub(crate) source: String,
    pub(crate) source_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InvocationProbeResult {
    pub(crate) candidates: Vec<InvocationProbeCandidate>,
    pub(crate) complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderInvocationProbe {
    pub(crate) source_ids: Vec<String>,
    pub(crate) files_read: usize,
    pub(crate) bytes_read: usize,
    pub(crate) complete: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct InvocationProbeBudget {
    max_files: usize,
    max_file_bytes: usize,
    max_total_bytes: usize,
    recent_window: Duration,
}

impl InvocationProbeBudget {
    fn remaining(self, files_read: usize, bytes_read: usize) -> Self {
        Self {
            max_files: self.max_files.saturating_sub(files_read),
            max_file_bytes: self.max_file_bytes,
            max_total_bytes: self.max_total_bytes.saturating_sub(bytes_read),
            recent_window: self.recent_window,
        }
    }
}

impl Default for InvocationProbeBudget {
    fn default() -> Self {
        Self {
            max_files: MAX_FILES,
            max_file_bytes: MAX_FILE_BYTES,
            max_total_bytes: MAX_TOTAL_BYTES,
            recent_window: RECENT_WINDOW,
        }
    }
}

pub(crate) fn probe_invocation_nonce(nonce: &str) -> InvocationProbeResult {
    if nonce.trim().is_empty() {
        return InvocationProbeResult { candidates: Vec::new(), complete: true };
    }
    let mut result = InvocationProbeResult { candidates: Vec::new(), complete: true };
    let mut budget = InvocationProbeBudget::default();
    let codex = super::codex::probe_invocation_nonce(nonce, budget);
    merge_provider_probe(&mut result, &mut budget, "codex", codex);
    let claude = super::claude_code::probe_invocation_nonce(nonce, budget);
    merge_provider_probe(&mut result, &mut budget, "claude-code", claude);
    result
}

fn merge_provider_probe(
    result: &mut InvocationProbeResult,
    budget: &mut InvocationProbeBudget,
    source: &str,
    probe: anyhow::Result<ProviderInvocationProbe>,
) {
    match probe {
        Ok(probe) => {
            *budget = budget.remaining(probe.files_read, probe.bytes_read);
            result.complete &= probe.complete;
            result.candidates.extend(probe.source_ids.into_iter().map(|source_id| {
                InvocationProbeCandidate { source: source.to_string(), source_id }
            }));
        }
        Err(_) => result.complete = false,
    }
}

pub(crate) fn probe_recent_files<F>(
    nonce: &str,
    entries: Vec<FileScanEntry>,
    budget: InvocationProbeBudget,
    confirms_input: F,
) -> ProviderInvocationProbe
where
    F: Fn(&serde_json::Value, &str) -> anyhow::Result<bool>,
{
    let cutoff = SystemTime::now().checked_sub(budget.recent_window);
    let mut recent = Vec::new();
    let mut complete = true;
    for entry in entries {
        let metadata = match entry.stat_target.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                complete = false;
                continue;
            }
        };
        let modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(_) => {
                complete = false;
                continue;
            }
        };
        if cutoff.is_some_and(|cutoff| modified < cutoff) {
            continue;
        }
        recent.push((modified, metadata.len(), entry));
    }
    recent.sort_by(|left, right| {
        right.0.cmp(&left.0).then_with(|| left.2.stat_target.cmp(&right.2.stat_target))
    });
    if recent.len() > budget.max_files {
        recent.truncate(budget.max_files);
        complete = false;
    }

    let mut files_read: usize = 0;
    let mut bytes_read: usize = 0;
    let mut source_ids = BTreeSet::new();
    for (_, file_len, entry) in recent {
        let read_len = usize::try_from(file_len).unwrap_or(usize::MAX).min(budget.max_file_bytes);
        if bytes_read.saturating_add(read_len) > budget.max_total_bytes {
            complete = false;
            break;
        }
        let mut file = match File::open(&entry.stat_target) {
            Ok(file) => file,
            Err(_) => {
                complete = false;
                continue;
            }
        };
        let start = file_len.saturating_sub(read_len as u64);
        if file.seek(SeekFrom::Start(start)).is_err() {
            complete = false;
            continue;
        }
        let mut bytes = Vec::with_capacity(read_len);
        if file.take(read_len as u64).read_to_end(&mut bytes).is_err() {
            complete = false;
            continue;
        }
        files_read += 1;
        bytes_read += bytes.len();
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => {
                complete = false;
                continue;
            }
        };
        let text = if start == 0 {
            text.as_str()
        } else {
            let Some(newline) = text.find('\n') else {
                if text.contains(nonce) {
                    complete = false;
                }
                continue;
            };
            let partial = &text[..newline];
            if partial.contains(nonce) {
                complete = false;
            }
            &text[newline + 1..]
        };
        if !text.contains(nonce) {
            continue;
        }
        for line in text.lines().filter(|line| line.contains(nonce)) {
            let value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            match confirms_input(&value, nonce) {
                Ok(true) => {
                    source_ids.insert(entry.session_id.clone());
                }
                Ok(false) => {}
                Err(_) => complete = false,
            }
        }
    }
    ProviderInvocationProbe {
        source_ids: source_ids.into_iter().collect(),
        files_read,
        bytes_read,
        complete,
    }
}

pub(crate) fn is_discovery_tool(name: &str) -> bool {
    matches!(name.rsplit("__").next(), Some("search_sessions" | "list_recent_sessions"))
}

pub(crate) fn nonce_matches_input(input: &serde_json::Value, nonce: &str) -> bool {
    input.get("invocation_nonce").and_then(serde_json::Value::as_str) == Some(nonce)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::Instant;

    use super::*;

    fn entry(path: PathBuf, session_id: &str) -> FileScanEntry {
        FileScanEntry { session_id: session_id.to_string(), stat_target: path, directory: None }
    }

    fn confirms(value: &serde_json::Value, nonce: &str) -> anyhow::Result<bool> {
        Ok(value.get("invocation_nonce").and_then(serde_json::Value::as_str) == Some(nonce))
    }

    #[test]
    fn unreadable_damaged_truncated_and_over_budget_inputs_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let damaged = root.path().join("damaged.jsonl");
        fs::write(&damaged, "{nonce-damaged\n").unwrap();
        let damaged_result = probe_recent_files(
            "nonce-damaged",
            vec![entry(damaged, "damaged")],
            InvocationProbeBudget::default(),
            confirms,
        );
        assert!(!damaged_result.complete);
        assert!(damaged_result.source_ids.is_empty());

        let missing = probe_recent_files(
            "nonce",
            vec![entry(root.path().join("missing.jsonl"), "missing")],
            InvocationProbeBudget::default(),
            confirms,
        );
        assert!(!missing.complete);

        let truncated = root.path().join("truncated.jsonl");
        fs::write(
            &truncated,
            format!("{}{{\"invocation_nonce\":\"nonce-cut\"}}\n", "x".repeat(64)),
        )
        .unwrap();
        let truncated_result = probe_recent_files(
            "nonce-cut",
            vec![entry(truncated, "truncated")],
            InvocationProbeBudget {
                max_files: 1,
                max_file_bytes: 32,
                max_total_bytes: 32,
                recent_window: RECENT_WINDOW,
            },
            confirms,
        );
        assert!(!truncated_result.complete);
        assert!(truncated_result.source_ids.is_empty());

        let first = root.path().join("first.jsonl");
        let second = root.path().join("second.jsonl");
        fs::write(&first, "{}\n").unwrap();
        fs::write(&second, "{}\n").unwrap();
        let over_budget = probe_recent_files(
            "nonce",
            vec![entry(first.clone(), "first"), entry(second.clone(), "second")],
            InvocationProbeBudget {
                max_files: 1,
                max_file_bytes: MAX_FILE_BYTES,
                max_total_bytes: MAX_TOTAL_BYTES,
                recent_window: RECENT_WINDOW,
            },
            confirms,
        );
        assert!(!over_budget.complete);

        let over_bytes = probe_recent_files(
            "nonce",
            vec![entry(first, "first"), entry(second, "second")],
            InvocationProbeBudget {
                max_files: 2,
                max_file_bytes: MAX_FILE_BYTES,
                max_total_bytes: 2,
                recent_window: RECENT_WINDOW,
            },
            confirms,
        );
        assert!(!over_bytes.complete);
        assert_eq!(over_bytes.bytes_read, 0);
    }

    #[test]
    fn remaining_budget_caps_aggregate_provider_work() {
        let budget = InvocationProbeBudget {
            max_files: 4,
            max_file_bytes: 32,
            max_total_bytes: 96,
            recent_window: RECENT_WINDOW,
        };
        let remaining = budget.remaining(3, 80);
        assert_eq!(remaining.max_files, 1);
        assert_eq!(remaining.max_file_bytes, 32);
        assert_eq!(remaining.max_total_bytes, 16);
    }

    #[test]
    fn fixed_fixture_probe_reports_stable_read_cost() {
        let root = tempfile::tempdir().unwrap();
        let nonce = "fixed-fixture-nonce";
        let mut entries = Vec::new();
        for index in 0..32 {
            let path = root.path().join(format!("{index:02}.jsonl"));
            let line = if index == 0 {
                serde_json::json!({"invocation_nonce": nonce}).to_string()
            } else {
                serde_json::json!({"content": "x".repeat(4096)}).to_string()
            };
            fs::write(&path, format!("{line}\n")).unwrap();
            entries.push(entry(path, &format!("session-{index:02}")));
        }

        let mut elapsed = Vec::new();
        let mut costs = Vec::new();
        for _ in 0..25 {
            let start = Instant::now();
            let result = probe_recent_files(
                nonce,
                entries.clone(),
                InvocationProbeBudget::default(),
                confirms,
            );
            elapsed.push(start.elapsed().as_micros());
            costs.push((result.files_read, result.bytes_read));
            assert!(result.complete);
            assert_eq!(result.source_ids, vec!["session-00".to_string()]);
        }
        elapsed.sort_unstable();
        assert!(costs.windows(2).all(|pair| pair[0] == pair[1]));
        eprintln!(
            "probe_cost runs=25 p50_us={} p95_us={} files={} bytes={}",
            elapsed[12], elapsed[23], costs[0].0, costs[0].1
        );
    }
}
