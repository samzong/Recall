use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::progress::Progress;

const BATCH_LEAVES: usize = 400;
const BATCH_CHARS: usize = 400_000;

#[derive(Clone, Debug)]
pub struct Leaf {
    pub pointer: String,
    pub record: usize,
    pub text: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Span {
    pub entity: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LeafRedaction {
    pub text: String,
    pub spans: Vec<Span>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RawFinding {
    pub leaf: usize,
    pub pointer: String,
    pub record: usize,
    pub entity: String,
    pub start: usize,
    pub end: usize,
    pub detector: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct Unresolved {
    pub pointer: String,
    pub record: usize,
    pub entity: String,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct ScanOutcome {
    pub redactions: BTreeMap<usize, LeafRedaction>,
    pub findings: Vec<RawFinding>,
    pub unresolved: Vec<Unresolved>,
    pub entities: BTreeMap<String, usize>,
}

pub fn marker(entity: &str) -> String {
    format!("[REDACTED:{entity}]")
}

pub fn gitleaks_entity(rule_id: &str) -> String {
    rule_id
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch.to_ascii_uppercase() } else { '_' })
        .collect()
}

#[derive(Debug, Deserialize)]
struct GitleaksFinding {
    #[serde(rename = "RuleID")]
    rule_id: String,
    #[serde(rename = "StartLine")]
    start_line: usize,
    #[serde(rename = "Secret")]
    secret: String,
}

struct ScanDocument {
    text: String,
    line_starts: Vec<usize>,
}

fn build_scan_document(leaves: &[Leaf]) -> ScanDocument {
    let mut text = String::new();
    let mut line_starts = Vec::with_capacity(leaves.len());
    let mut line = 1usize;
    for leaf in leaves {
        line_starts.push(line);
        text.push_str(&leaf.text);
        text.push('\n');
        line += leaf.text.matches('\n').count() + 1;
    }
    ScanDocument { text, line_starts }
}

fn leaf_for_line(line_starts: &[usize], line: usize) -> Option<usize> {
    match line_starts.binary_search(&line) {
        Ok(index) => Some(index),
        Err(0) => None,
        Err(index) => Some(index - 1),
    }
}

pub fn run_gitleaks(
    leaves: &[Leaf],
    config: &Config,
) -> Result<(Vec<RawFinding>, Vec<Unresolved>)> {
    if leaves.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let document = build_scan_document(leaves);
    let mut child = Command::new("gitleaks")
        .args([
            "stdin",
            "--no-banner",
            "--report-format",
            "json",
            "--report-path",
            "-",
            "--exit-code",
            "0",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run gitleaks; run `recall publish doctor`")?;
    child
        .stdin
        .take()
        .context("gitleaks did not accept input")?
        .write_all(document.text.as_bytes())
        .context("failed to send the scan document to gitleaks")?;
    let output = child.wait_with_output().context("gitleaks did not complete")?;
    if !output.status.success() {
        bail!("gitleaks failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let findings: Vec<GitleaksFinding> =
        serde_json::from_slice(&output.stdout).context("gitleaks did not return a JSON report")?;

    let mut resolved = Vec::new();
    let mut unresolved = Vec::new();
    for finding in findings {
        let entity = gitleaks_entity(&finding.rule_id);
        if config.allow.gitleaks_rules.iter().any(|rule| rule == &finding.rule_id) {
            continue;
        }
        let Some(index) = leaf_for_line(&document.line_starts, finding.start_line) else {
            unresolved.push(Unresolved {
                pointer: String::new(),
                record: 0,
                entity,
                reason: format!(
                    "gitleaks reported line {} outside the scan document",
                    finding.start_line
                ),
            });
            continue;
        };
        let leaf = &leaves[index];
        let mut matched = false;
        for (byte_start, _) in leaf.text.match_indices(&finding.secret) {
            matched = true;
            let start = leaf.text[..byte_start].chars().count();
            resolved.push(RawFinding {
                leaf: index,
                pointer: leaf.pointer.clone(),
                record: leaf.record,
                entity: entity.clone(),
                start,
                end: start + finding.secret.chars().count(),
                detector: "gitleaks",
            });
        }
        if !matched {
            unresolved.push(Unresolved {
                pointer: leaf.pointer.clone(),
                record: leaf.record,
                entity,
                reason: "gitleaks matched across a scan-document boundary".to_string(),
            });
        }
    }
    Ok((resolved, unresolved))
}

#[derive(Serialize)]
struct BridgeRequest<'a> {
    languages: &'a [String],
    allow_identities: &'a [String],
    allow_entities: &'a [String],
    leaves: Vec<BridgeLeaf<'a>>,
    spans: Vec<BridgeSpan<'a>>,
}

#[derive(Serialize)]
struct BridgeLeaf<'a> {
    i: usize,
    text: &'a str,
}

#[derive(Serialize)]
struct BridgeSpan<'a> {
    i: usize,
    start: usize,
    end: usize,
    entity: &'a str,
}

#[derive(Deserialize)]
struct BridgeResponse {
    #[serde(default)]
    leaves: Vec<BridgeLeafResult>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct BridgeLeafResult {
    i: usize,
    text: String,
    #[serde(default)]
    spans: Vec<BridgeSpanResult>,
}

#[derive(Deserialize)]
struct BridgeSpanResult {
    entity: String,
    start: usize,
    end: usize,
}

pub struct Bridge {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Bridge {
    pub fn start(env_dir: &Path) -> Result<Self> {
        let script = env_dir.join("scan.py");
        if !script.is_file() {
            bail!(
                "the redaction environment is missing at {}; run `recall publish doctor --install`",
                env_dir.display()
            );
        }
        let mut child = Command::new("uv")
            .arg("run")
            .arg("--project")
            .arg(env_dir)
            .arg("--quiet")
            .arg("python")
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("failed to start the redaction environment; run `recall publish doctor`")?;
        let stdin = child.stdin.take().context("redaction environment refused input")?;
        let stdout = BufReader::new(
            child.stdout.take().context("redaction environment produced no output")?,
        );
        Ok(Self { child, stdin, stdout })
    }

    fn exchange(&mut self, request: &BridgeRequest<'_>) -> Result<BridgeResponse> {
        let line = serde_json::to_string(request)?;
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        let mut response = String::new();
        if self.stdout.read_line(&mut response)? == 0 {
            bail!("the redaction environment exited before answering");
        }
        let response: BridgeResponse = serde_json::from_str(response.trim())
            .context("the redaction environment returned invalid JSON")?;
        if let Some(error) = response.error {
            bail!("redaction failed: {error}");
        }
        Ok(response)
    }

    pub fn finish(mut self) -> Result<()> {
        drop(self.stdin);
        let status = self.child.wait().context("the redaction environment did not exit")?;
        if !status.success() {
            bail!("the redaction environment exited with {status}");
        }
        Ok(())
    }
}

pub fn scan(
    env_dir: &Path,
    leaves: &[Leaf],
    config: &Config,
    progress: &Progress,
) -> Result<ScanOutcome> {
    let mut outcome = ScanOutcome::default();
    if leaves.is_empty() {
        return Ok(outcome);
    }
    progress.step(&format!("Scanning {} text fields for secrets", leaves.len()));
    let (findings, unresolved) = run_gitleaks(leaves, config)?;
    progress.step(&format!("gitleaks reported {} finding(s)", findings.len()));
    outcome.unresolved = unresolved;
    outcome.findings = findings;

    let mut spans_by_leaf: BTreeMap<usize, Vec<&RawFinding>> = BTreeMap::new();
    for finding in &outcome.findings {
        spans_by_leaf.entry(finding.leaf).or_default().push(finding);
    }

    progress.step("Loading the language models; the first batch takes the longest");
    let mut bridge = Bridge::start(env_dir)?;
    let mut index = 0usize;
    let mut scanned = 0usize;
    while index < leaves.len() {
        let mut batch = Vec::new();
        let mut spans = Vec::new();
        let mut chars = 0usize;
        while index < leaves.len() && batch.len() < BATCH_LEAVES && chars < BATCH_CHARS {
            let leaf = &leaves[index];
            batch.push(BridgeLeaf { i: index, text: &leaf.text });
            if let Some(found) = spans_by_leaf.get(&index) {
                for finding in found {
                    spans.push(BridgeSpan {
                        i: index,
                        start: finding.start,
                        end: finding.end,
                        entity: &finding.entity,
                    });
                }
            }
            chars += leaf.text.len();
            index += 1;
        }
        let request = BridgeRequest {
            languages: &config.languages,
            allow_identities: &config.allow.identities,
            allow_entities: &config.allow.presidio_entities,
            leaves: batch,
            spans,
        };
        scanned += request.leaves.len();
        progress.tick(&format!("Redacting identities: {scanned}/{} fields", leaves.len()));
        let response = bridge.exchange(&request)?;
        for result in response.leaves {
            if result.spans.is_empty() {
                continue;
            }
            let leaf = leaves
                .get(result.i)
                .ok_or_else(|| anyhow!("redaction returned an unknown leaf index"))?;
            for span in &result.spans {
                *outcome.entities.entry(span.entity.clone()).or_default() += 1;
            }
            outcome.redactions.insert(
                result.i,
                LeafRedaction {
                    text: result.text,
                    spans: result
                        .spans
                        .into_iter()
                        .map(|span| Span { entity: span.entity, start: span.start, end: span.end })
                        .collect(),
                },
            );
            let _ = leaf;
        }
    }
    bridge.finish()?;
    progress.step(&format!("Redacted {} field(s)", outcome.redactions.len()));
    Ok(outcome)
}

pub fn environment_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("publish").join("env")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(pointer: &str, text: &str) -> Leaf {
        Leaf { pointer: pointer.to_string(), record: 0, text: text.to_string() }
    }

    #[test]
    fn scan_document_line_index_maps_findings_back_to_their_leaf() {
        let leaves = vec![leaf("/a", "one"), leaf("/b", "two\nthree\nfour"), leaf("/c", "five")];
        let document = build_scan_document(&leaves);
        assert_eq!(document.line_starts, vec![1, 2, 5]);
        assert_eq!(leaf_for_line(&document.line_starts, 1), Some(0));
        assert_eq!(leaf_for_line(&document.line_starts, 3), Some(1));
        assert_eq!(leaf_for_line(&document.line_starts, 4), Some(1));
        assert_eq!(leaf_for_line(&document.line_starts, 5), Some(2));
        assert_eq!(leaf_for_line(&document.line_starts, 0), None);
    }

    #[test]
    fn multi_line_leaves_keep_real_newlines_so_block_rules_can_match() {
        let leaves = vec![leaf("/a", "-----BEGIN KEY-----\nbody\n-----END KEY-----")];
        let document = build_scan_document(&leaves);
        assert!(document.text.contains("-----BEGIN KEY-----\nbody"));
        assert!(!document.text.contains("\\n"));
    }

    #[test]
    fn secret_offsets_cross_the_bridge_as_character_positions() {
        let text = "密钥是 ghp_0123456789abcdefghijklmnopqrstuvwxyz 请撤销";
        let secret = "ghp_0123456789abcdefghijklmnopqrstuvwxyz";
        let byte_start = text.find(secret).unwrap();
        let char_start = text[..byte_start].chars().count();
        assert_ne!(byte_start, char_start);
        assert_eq!(
            text.chars().skip(char_start).take(secret.chars().count()).collect::<String>(),
            secret
        );
    }

    #[test]
    fn rule_identifiers_become_stable_entity_markers() {
        assert_eq!(gitleaks_entity("github-pat"), "GITHUB_PAT");
        assert_eq!(gitleaks_entity("aws-access-token"), "AWS_ACCESS_TOKEN");
        assert_eq!(marker("GITHUB_PAT"), "[REDACTED:GITHUB_PAT]");
    }
}
