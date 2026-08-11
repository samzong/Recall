//! Benchmark surface for the targets in `benches/`.
//!
//! Every module of this crate is `pub(crate)`, so the benchmark binaries — which
//! link the library as an external crate — cannot reach the hot paths directly.
//! This module is compiled only with the `bench` feature and exposes fixtures
//! that build deterministic workloads and drive the real production code.
//!
//! Everything here is intentionally side-effect free: transcripts are written to
//! a temporary directory and the index lives in an in-memory SQLite database, so
//! benchmarks never touch the user's `recall.db`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::json;
use tempfile::TempDir;

use crate::adapters::{claude_code, codex};
use crate::db::search::{SearchEngine, SearchFilters, TimeRange};
use crate::db::store::{SessionTopologyWrite, Store};
use crate::export::{ExportIncludes, ExportOptions};
use crate::project_scope::ProjectScope;
use crate::share::meta::SessionDisplayMeta;
use crate::share::render;
use crate::types::{Message, Role, Session, UsageEventRecord};
use crate::{db, export, repo_identity, semantic, transcript, usage};

/// Width of the `message_vec` virtual table.
pub const EMBEDDING_DIM: usize = 384;

/// Deterministic timestamp base (2025-01-01T00:00:00Z) so fixtures never drift.
const BASE_TIMESTAMP_MS: i64 = 1_735_689_600_000;

const MODELS: &[&str] = &["claude-sonnet-4-5", "claude-opus-4-1", "gpt-5-codex", "gemini-2.5-pro"];
const TOOLS: &[&str] = &["Read", "Edit", "Bash", "Grep", "WebFetch"];
const WORDS: &[&str] = &[
    "session",
    "transcript",
    "adapter",
    "sqlite",
    "embedding",
    "vector",
    "index",
    "query",
    "snippet",
    "token",
    "usage",
    "cache",
    "provider",
    "workspace",
    "directory",
    "repository",
    "commit",
    "rollout",
    "summary",
    "message",
    "assistant",
    "prompt",
    "context",
    "window",
    "latency",
    "throughput",
    "regression",
    "baseline",
    "migration",
    "schema",
    "parser",
    "handoff",
    "semantic",
    "keyword",
    "hybrid",
    "dedupe",
    "aggregate",
    "timeline",
    "sidechain",
    "subagent",
];

/// Small xorshift generator: no extra dependency and identical data every run.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }

    fn pick(&mut self, items: &'static [&'static str]) -> &'static str {
        items[self.below(items.len())]
    }
}

fn words(rng: &mut Rng, count: usize) -> String {
    let mut out = String::with_capacity(count * 8);
    for index in 0..count {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(rng.pick(WORDS));
    }
    out
}

/// A user turn: a couple of sentences, the shape most prompts have.
fn user_prompt(rng: &mut Rng) -> String {
    format!("{}?\n\n{}.", words(rng, 12), words(rng, 24))
}

/// An assistant turn: prose, a bullet list and a fenced code block, which is
/// what the markdown renderer has to deal with in practice.
fn assistant_markdown(rng: &mut Rng) -> String {
    format!(
        "## {}\n\n{}.\n\n- {}\n- {}\n- {}\n\n```rust\nfn {}() -> usize {{\n    // {}\n    {}\n}}\n```\n\n{}.",
        words(rng, 4),
        words(rng, 40),
        words(rng, 8),
        words(rng, 8),
        words(rng, 8),
        rng.pick(WORDS),
        words(rng, 10),
        rng.below(4096),
        words(rng, 30),
    )
}

fn rfc3339(millis: i64) -> String {
    chrono::DateTime::from_timestamp_millis(millis)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// What a transcript parse produced, so benchmarks can black-box a real result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedCounts {
    pub messages: usize,
    pub usage_events: usize,
    pub events: usize,
}

/// A synthetic transcript on disk, in the JSONL dialect of a given source.
pub struct Transcript {
    _dir: TempDir,
    path: PathBuf,
}

impl Transcript {
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write(name: &str, lines: &[String]) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join(name);
        let mut file = std::fs::File::create(&path).expect("create transcript");
        for line in lines {
            writeln!(file, "{line}").expect("write transcript");
        }
        file.sync_all().expect("flush transcript");
        Self { _dir: dir, path }
    }

    /// Claude Code transcript: `{user,assistant}` records with content blocks,
    /// tool calls and per-message `usage` payloads.
    pub fn claude(turns: usize) -> Self {
        let mut rng = Rng::new(0x5eed_1234);
        let session_id = "6f1c3ba4-1f42-4c1a-9c2f-0f9a51d2b7e1";
        let mut lines = Vec::with_capacity(turns * 2 + 2);
        lines.push(
            json!({
                "type": "summary",
                "summary": words(&mut rng, 10),
                "leafUuid": session_id,
            })
            .to_string(),
        );
        for turn in 0..turns {
            let ts = BASE_TIMESTAMP_MS + turn as i64 * 45_000;
            lines.push(
                json!({
                    "type": "user",
                    "sessionId": session_id,
                    "cwd": "/home/dev/projects/recall",
                    "timestamp": rfc3339(ts),
                    "message": { "role": "user", "content": user_prompt(&mut rng) },
                })
                .to_string(),
            );
            let tool = rng.pick(TOOLS).to_string();
            lines.push(
                json!({
                    "type": "assistant",
                    "sessionId": session_id,
                    "requestId": format!("req_{turn}"),
                    "timestamp": rfc3339(ts + 12_000),
                    "message": {
                        "id": format!("msg_{turn}"),
                        "role": "assistant",
                        "model": rng.pick(MODELS),
                        "content": [
                            { "type": "text", "text": assistant_markdown(&mut rng) },
                            {
                                "type": "tool_use",
                                "id": format!("toolu_{turn}"),
                                "name": tool,
                                "input": { "file_path": "/home/dev/projects/recall/src/db/search.rs", "pattern": words(&mut rng, 3) },
                            },
                        ],
                        "usage": {
                            "input_tokens": 1_200 + rng.below(800),
                            "output_tokens": 400 + rng.below(600),
                            "cache_read_input_tokens": 20_000 + rng.below(10_000),
                            "cache_creation_input_tokens": rng.below(4_000),
                        },
                    },
                })
                .to_string(),
            );
            lines.push(
                json!({
                    "type": "user",
                    "sessionId": session_id,
                    "timestamp": rfc3339(ts + 14_000),
                    "message": {
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": format!("toolu_{turn}"),
                            "content": [{ "type": "text", "text": words(&mut rng, 60) }],
                        }],
                    },
                })
                .to_string(),
            );
        }
        Self::write(&format!("{session_id}.jsonl"), &lines)
    }

    /// Codex rollout: `session_meta`, `event_msg` and `response_item` records
    /// with cumulative `token_count` events.
    pub fn codex(turns: usize) -> Self {
        let mut rng = Rng::new(0xc0de_9876);
        let session_id = "019e6d8d-588b-7fd2-a326-c525469ed120";
        let mut lines = Vec::with_capacity(turns * 4 + 1);
        lines.push(
            json!({
                "timestamp": rfc3339(BASE_TIMESTAMP_MS),
                "type": "session_meta",
                "payload": {
                    "id": session_id,
                    "timestamp": rfc3339(BASE_TIMESTAMP_MS),
                    "cwd": "/home/dev/projects/recall",
                    "model": "gpt-5-codex",
                    "model_provider": "openai",
                },
            })
            .to_string(),
        );
        let mut input_total = 0i64;
        let mut output_total = 0i64;
        let mut cached_total = 0i64;
        let mut reasoning_total = 0i64;
        for turn in 0..turns {
            let ts = BASE_TIMESTAMP_MS + turn as i64 * 45_000;
            lines.push(
                json!({
                    "timestamp": rfc3339(ts),
                    "type": "event_msg",
                    "payload": { "type": "user_message", "message": user_prompt(&mut rng) },
                })
                .to_string(),
            );
            lines.push(
                json!({
                    "timestamp": rfc3339(ts + 9_000),
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": assistant_markdown(&mut rng) }],
                    },
                })
                .to_string(),
            );
            lines.push(
                json!({
                    "timestamp": rfc3339(ts + 11_000),
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "shell",
                        "arguments": json!({ "command": ["bash", "-lc", words(&mut rng, 6)] }).to_string(),
                        "call_id": format!("call_{turn}"),
                    },
                })
                .to_string(),
            );
            lines.push(
                json!({
                    "timestamp": rfc3339(ts + 13_000),
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": format!("call_{turn}"),
                        "output": words(&mut rng, 40),
                    },
                })
                .to_string(),
            );
            input_total += 900 + rng.below(600) as i64;
            output_total += 300 + rng.below(400) as i64;
            cached_total += 5_000 + rng.below(2_000) as i64;
            reasoning_total += rng.below(500) as i64;
            lines.push(
                json!({
                    "timestamp": rfc3339(ts + 14_000),
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "total_token_usage": {
                                "input_tokens": input_total,
                                "cached_input_tokens": cached_total,
                                "output_tokens": output_total,
                                "reasoning_output_tokens": reasoning_total,
                                "total_tokens": input_total + output_total + cached_total,
                            },
                            "last_token_usage": {
                                "input_tokens": 900,
                                "cached_input_tokens": 5_000,
                                "output_tokens": 300,
                                "reasoning_output_tokens": 100,
                                "total_tokens": 6_300,
                            },
                            "model_context_window": 272_000,
                        },
                    },
                })
                .to_string(),
            );
        }
        Self::write(&format!("rollout-2025-01-01T00-00-00-{session_id}.jsonl"), &lines)
    }

    /// Full Claude Code transcript parse, the `recall sync` hot path.
    pub fn parse_claude(&self, include_events: bool) -> ParsedCounts {
        let parsed =
            claude_code::parse_conversation_jsonl(&self.path, BASE_TIMESTAMP_MS, include_events)
                .expect("parse claude transcript");
        ParsedCounts {
            messages: parsed.messages.len(),
            usage_events: parsed.usage_events.len(),
            events: parsed.events.len(),
        }
    }

    /// Full Codex rollout parse, the `recall sync` hot path.
    pub fn parse_codex(&self, include_events: bool) -> ParsedCounts {
        let parsed = codex::parse_codex_session_with_options(&self.path, include_events)
            .expect("parse codex rollout")
            .expect("non-empty codex rollout");
        ParsedCounts {
            messages: parsed.messages.len(),
            usage_events: parsed.usage_events.len(),
            events: parsed.events.len(),
        }
    }
}

fn synthetic_session(index: usize, message_count: usize) -> Session {
    let mut rng = Rng::new(0xa11ce ^ index as u64);
    Session {
        id: format!("claude-code:session-{index}"),
        source: "claude-code".to_string(),
        source_id: format!("session-{index}"),
        title: words(&mut rng, 6),
        directory: Some(format!("/home/dev/projects/project-{}", index % 16)),
        repo_remote: Some("github.com/samzong/recall".to_string()),
        repo_slug: Some("samzong/recall".to_string()),
        repo_name: Some("recall".to_string()),
        started_at: BASE_TIMESTAMP_MS - (index as i64 * 3_600_000),
        updated_at: Some(BASE_TIMESTAMP_MS - (index as i64 * 3_600_000) + 900_000),
        message_count: message_count as u32,
        entrypoint: Some("cli".to_string()),
        custom_title: None,
        summary: Some(words(&mut rng, 12)),
        duration_minutes: Some(15),
        source_file_path: Some(format!("/home/dev/.claude/projects/p/session-{index}.jsonl")),
        is_import: false,
    }
}

fn synthetic_messages(session: &Session, count: usize) -> Vec<Message> {
    let mut rng = Rng::new(0xb0b ^ session.started_at as u64);
    (0..count)
        .map(|seq| {
            let role = if seq % 2 == 0 { Role::User } else { Role::Assistant };
            let content = match role {
                Role::User => user_prompt(&mut rng),
                Role::Assistant => assistant_markdown(&mut rng),
            };
            Message {
                session_id: session.id.clone(),
                role,
                content,
                timestamp: Some(session.started_at + seq as i64 * 30_000),
                seq: seq as u32,
            }
        })
        .collect()
}

fn embedding(seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut values: Vec<f32> =
        (0..EMBEDDING_DIM).map(|_| rng.below(2_000) as f32 / 1_000.0 - 1.0).collect();
    let norm = values.iter().map(|v| v * v).sum::<f32>().sqrt().max(f32::EPSILON);
    for value in &mut values {
        *value /= norm;
    }
    values
}

/// An empty in-memory index, schema already migrated.
pub struct BenchStore {
    store: Store,
}

impl BenchStore {
    pub fn empty() -> Self {
        db::schema::register_sqlite_vec();
        Self { store: Store::open_in_memory().expect("in-memory store") }
    }
}

/// Sessions and messages waiting to be written to the index, the write half of
/// `recall sync` (session upsert, message insert, FTS5 index maintenance).
pub struct IndexWorkload {
    sessions: Vec<(Session, Vec<Message>)>,
}

impl IndexWorkload {
    pub fn generate(sessions: usize, messages_per_session: usize) -> Self {
        Self {
            sessions: (0..sessions)
                .map(|index| {
                    let session = synthetic_session(index, messages_per_session);
                    let messages = synthetic_messages(&session, messages_per_session);
                    (session, messages)
                })
                .collect(),
        }
    }

    pub fn persist(&self, target: &BenchStore) -> usize {
        let mut written = 0;
        for (session, messages) in &self.sessions {
            target
                .store
                .persist_session_with_usage_and_events_with_topology(
                    session,
                    messages,
                    &[],
                    None,
                    &[],
                    None,
                    &SessionTopologyWrite::none(),
                )
                .expect("persist session");
            written += messages.len();
        }
        written
    }
}

/// A populated index used to benchmark the read paths: keyword search, hybrid
/// (FTS + vector) search and JSONL export.
pub struct SearchIndex {
    store: Store,
    query_embedding: Vec<f32>,
}

impl SearchIndex {
    pub fn build(sessions: usize, messages_per_session: usize, with_vectors: bool) -> Self {
        let target = BenchStore::empty();
        IndexWorkload::generate(sessions, messages_per_session).persist(&target);
        let store = target.store;
        if with_vectors {
            for index in 0..sessions {
                let session_id = format!("claude-code:session-{index}");
                let messages = store.embeddable_messages(&session_id).expect("embeddable messages");
                let vectors: Vec<(i64, Vec<f32>)> =
                    messages.iter().map(|(id, _)| (*id, embedding(*id as u64 + 7))).collect();
                let items: Vec<(i64, &[f32])> =
                    vectors.iter().map(|(id, vector)| (*id, vector.as_slice())).collect();
                store.upsert_embeddings(&items).expect("upsert embeddings");
            }
        }
        Self { store, query_embedding: embedding(42) }
    }

    fn filters() -> SearchFilters {
        SearchFilters {
            sources: None,
            time_range: TimeRange::All,
            scope: ProjectScope::Global,
            thread_role: None,
        }
    }

    /// FTS5-only search, what `recall search` runs without a local model.
    pub fn keyword_search(&self, query: &str) -> usize {
        SearchEngine::new(&self.store.conn)
            .hybrid_search(query, None, &Self::filters(), 20, 3)
            .expect("keyword search")
            .len()
    }

    /// FTS5 + sqlite-vec reciprocal-rank fusion, the default `recall search`.
    pub fn hybrid_search(&self, query: &str) -> usize {
        SearchEngine::new(&self.store.conn)
            .hybrid_search(query, Some(&self.query_embedding), &Self::filters(), 20, 3)
            .expect("hybrid search")
            .len()
    }

    /// `recall export`: read every session back out and serialize it as JSONL.
    pub fn export_jsonl(&self) -> usize {
        let options = ExportOptions {
            session_ids: Vec::new(),
            sources: None,
            time_range: TimeRange::All,
            scope: ProjectScope::Global,
            thread_role: None,
            limit: None,
            includes: ExportIncludes::full(),
        };
        let mut buffer = Vec::with_capacity(1 << 16);
        export::write_jsonl(&self.store, &options, &mut buffer).expect("export jsonl");
        buffer.len()
    }
}

/// Usage events feeding the `recall usage` dashboard aggregation.
pub struct UsageWorkload {
    events: Vec<UsageEventRecord>,
}

impl UsageWorkload {
    pub fn generate(events: usize) -> Self {
        let mut rng = Rng::new(0xfeed_beef);
        Self {
            events: (0..events)
                .map(|index| {
                    let session = index / 24;
                    UsageEventRecord {
                        session_id: format!("claude-code:session-{session}"),
                        source: if index % 3 == 0 { "codex" } else { "claude-code" }.to_string(),
                        source_id: format!("session-{session}"),
                        event_key: format!("assistant:req_{index}:msg_{index}"),
                        timestamp: BASE_TIMESTAMP_MS - index as i64 * 600_000,
                        model: rng.pick(MODELS).to_string(),
                        provider: if index % 3 == 0 { "openai" } else { "anthropic" }.to_string(),
                        input_tokens: 900 + rng.below(900) as i64,
                        output_tokens: 300 + rng.below(500) as i64,
                        cache_read_tokens: 12_000 + rng.below(8_000) as i64,
                        cache_write_tokens: rng.below(3_000) as i64,
                        reasoning_tokens: rng.below(700) as i64,
                        token_source: if index % 3 == 0 { "derived" } else { "observed" }
                            .to_string(),
                    }
                })
                .collect(),
        }
    }

    /// Dedupe + group by source, model, day, week and month.
    pub fn aggregate(&self) -> i64 {
        usage::aggregate_usage_events(&self.events).summary.tokens.total_tokens
    }
}

/// A session ready to be rendered as plain text or as a shareable HTML page.
pub struct RenderWorkload {
    session: Session,
    messages: Vec<Message>,
    display_meta: SessionDisplayMeta,
}

impl RenderWorkload {
    pub fn generate(messages: usize) -> Self {
        let session = synthetic_session(0, messages);
        let messages = synthetic_messages(&session, messages);
        Self {
            session,
            messages,
            display_meta: SessionDisplayMeta {
                models: vec!["claude-sonnet-4-5".to_string()],
                thinking_depths: vec!["high".to_string()],
            },
        }
    }

    /// `recall session show`: plain-text transcript.
    pub fn render_plain(&self) -> usize {
        transcript::render_plain(&self.session, &self.messages).len()
    }

    /// `recall session share`: markdown to a self-contained HTML page.
    pub fn render_html(&self) -> usize {
        render::render_session_html(&self.session, &self.messages, &self.display_meta).len()
    }
}

/// Prompt-to-embedding-input normalization, called once per indexed message.
pub fn build_embedding_text(title: &str, content: &str) -> String {
    semantic::build_embedding_text(title, content)
}

/// Git remote normalization, called once per session directory during sync.
pub fn normalize_remote_url(url: &str) -> Option<String> {
    repo_identity::normalize_remote_url(url).map(|identity| identity.slug)
}
