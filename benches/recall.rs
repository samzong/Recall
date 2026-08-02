//! CodSpeed benchmarks for Recall's hot paths.
//!
//! The groups follow the pipeline a session goes through: transcripts are
//! parsed (`parsing`), written to the SQLite index (`indexing`), read back for
//! search and export (`search`), aggregated for the usage dashboard
//! (`analytics`) and finally rendered (`rendering`).
//!
//! Run locally with:
//!
//! ```sh
//! cargo codspeed build --features bench
//! cargo codspeed run
//! ```

use recall::bench_api::{
    BenchStore, IndexWorkload, RenderWorkload, SearchIndex, Transcript, UsageWorkload,
    build_embedding_text, normalize_remote_url,
};

fn main() {
    divan::main();
}

/// Turning raw agent transcripts into sessions, messages, usage and events.
mod parsing {
    use super::*;

    /// Turn counts spanning a short session up to a long working session.
    const TURNS: &[usize] = &[8, 64, 256];

    #[divan::bench(args = TURNS)]
    fn claude_code(bencher: divan::Bencher, turns: usize) {
        let transcript = Transcript::claude(turns);
        bencher.bench(|| divan::black_box(transcript.parse_claude(true)));
    }

    /// Same transcript without the tool-call event stream, the path taken when
    /// only search and usage data is refreshed.
    #[divan::bench]
    fn claude_code_messages_only(bencher: divan::Bencher) {
        let transcript = Transcript::claude(64);
        bencher.bench(|| divan::black_box(transcript.parse_claude(false)));
    }

    #[divan::bench(args = TURNS)]
    fn codex(bencher: divan::Bencher, turns: usize) {
        let transcript = Transcript::codex(turns);
        bencher.bench(|| divan::black_box(transcript.parse_codex(true)));
    }
}

/// Writing sessions into SQLite, including FTS5 index maintenance.
mod indexing {
    use super::*;

    #[divan::bench(args = [8, 64])]
    fn persist_sessions(bencher: divan::Bencher, sessions: usize) {
        let workload = IndexWorkload::generate(sessions, 24);
        bencher
            .with_inputs(BenchStore::empty)
            .bench_local_refs(|store| divan::black_box(workload.persist(store)));
    }

    /// One long session: message insert throughput rather than session upserts.
    #[divan::bench]
    fn persist_long_session(bencher: divan::Bencher) {
        let workload = IndexWorkload::generate(1, 512);
        bencher
            .with_inputs(BenchStore::empty)
            .bench_local_refs(|store| divan::black_box(workload.persist(store)));
    }
}

/// Reading the index back: search and export.
mod search {
    use super::*;

    #[divan::bench]
    fn keyword_single_term(bencher: divan::Bencher) {
        let index = SearchIndex::build(64, 24, false);
        bencher.bench_local(|| divan::black_box(index.keyword_search("embedding")));
    }

    #[divan::bench]
    fn keyword_multi_term(bencher: divan::Bencher) {
        let index = SearchIndex::build(64, 24, false);
        bencher.bench_local(|| divan::black_box(index.keyword_search("sqlite vector index")));
    }

    #[divan::bench]
    fn hybrid_fts_and_vector(bencher: divan::Bencher) {
        let index = SearchIndex::build(64, 24, true);
        bencher.bench_local(|| divan::black_box(index.hybrid_search("sqlite vector index")));
    }

    #[divan::bench]
    fn export_jsonl(bencher: divan::Bencher) {
        let index = SearchIndex::build(32, 24, false);
        bencher.bench_local(|| divan::black_box(index.export_jsonl()));
    }
}

/// Usage dashboard aggregation and the per-message helpers around it.
mod analytics {
    use super::*;

    #[divan::bench(args = [1_000, 10_000])]
    fn aggregate_usage(bencher: divan::Bencher, events: usize) {
        let workload = UsageWorkload::generate(events);
        bencher.bench(|| divan::black_box(workload.aggregate()));
    }

    #[divan::bench]
    fn embedding_text() {
        let content = "\
            Recall keeps every local coding session in one SQLite index, then \
            layers FTS5 keyword search and sqlite-vec similarity on top of it.";
        divan::black_box(build_embedding_text(divan::black_box("Index a long session"), content));
    }

    #[divan::bench]
    fn remote_url_normalization() {
        divan::black_box(normalize_remote_url(divan::black_box(
            "git@github.com:samzong/Recall.git",
        )));
    }
}

/// Rendering a session for the CLI and for the shareable HTML page.
mod rendering {
    use super::*;

    #[divan::bench(args = [32, 256])]
    fn plain_transcript(bencher: divan::Bencher, messages: usize) {
        let workload = RenderWorkload::generate(messages);
        bencher.bench(|| divan::black_box(workload.render_plain()));
    }

    #[divan::bench(args = [32, 256])]
    fn share_html(bencher: divan::Bencher, messages: usize) {
        let workload = RenderWorkload::generate(messages);
        bencher.bench(|| divan::black_box(workload.render_html()));
    }
}
