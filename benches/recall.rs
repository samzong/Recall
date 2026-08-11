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

/// Under CodSpeed the benchmarked closure runs exactly once between the
/// instrumentation start and stop, with no warm-up, so first-execution costs
/// are measured as if they were steady-state work: page faults for a freshly
/// grown buffer, an allocator that has not yet seen this size class, cold
/// caches and branch predictors. Those costs vary with the environment rather
/// than with the code, which is what made `plain_transcript[256]` — a 167 KB
/// output, past the mmap threshold — swing by 25% between runs of identical
/// code and produce false regressions.
///
/// Running the same work once before the measured region moves those costs out
/// of it. Every benchmark whose work is side-effect free does this; the numbers
/// it reports are therefore steady-state, not first-call.
fn warm_up<T, F: Fn() -> T>(work: &F) {
    divan::black_box(work());
}

/// Turning raw agent transcripts into sessions, messages, usage and events.
mod parsing {
    use super::*;

    /// Turn counts spanning a short session up to a long working session.
    const TURNS: &[usize] = &[8, 64, 256];

    #[divan::bench(args = TURNS)]
    fn claude_code(bencher: divan::Bencher, turns: usize) {
        let transcript = Transcript::claude(turns);
        let parse = || divan::black_box(transcript.parse_claude(true));
        warm_up(&parse);
        bencher.bench(parse);
    }

    /// Same transcript without the tool-call event stream, the path taken when
    /// only search and usage data is refreshed.
    #[divan::bench]
    fn claude_code_messages_only(bencher: divan::Bencher) {
        let transcript = Transcript::claude(64);
        let parse = || divan::black_box(transcript.parse_claude(false));
        warm_up(&parse);
        bencher.bench(parse);
    }

    #[divan::bench(args = TURNS)]
    fn codex(bencher: divan::Bencher, turns: usize) {
        let transcript = Transcript::codex(turns);
        let parse = || divan::black_box(transcript.parse_codex(true));
        warm_up(&parse);
        bencher.bench(parse);
    }
}

/// Writing sessions into SQLite, including FTS5 index maintenance.
mod indexing {
    use super::*;

    /// The measured store is generated per iteration, so the warm-up needs its
    /// own throwaway store rather than the one being measured.
    fn warm_up_persist(workload: &IndexWorkload) {
        let store = BenchStore::empty();
        divan::black_box(workload.persist(&store));
    }

    #[divan::bench(args = [8, 64])]
    fn persist_sessions(bencher: divan::Bencher, sessions: usize) {
        let workload = IndexWorkload::generate(sessions, 24);
        warm_up_persist(&workload);
        bencher
            .with_inputs(BenchStore::empty)
            .bench_local_refs(|store| divan::black_box(workload.persist(store)));
    }

    /// One long session: message insert throughput rather than session upserts.
    #[divan::bench]
    fn persist_long_session(bencher: divan::Bencher) {
        let workload = IndexWorkload::generate(1, 512);
        warm_up_persist(&workload);
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
        let search = || divan::black_box(index.keyword_search("embedding"));
        warm_up(&search);
        bencher.bench_local(search);
    }

    #[divan::bench]
    fn keyword_multi_term(bencher: divan::Bencher) {
        let index = SearchIndex::build(64, 24, false);
        let search = || divan::black_box(index.keyword_search("sqlite vector index"));
        warm_up(&search);
        bencher.bench_local(search);
    }

    #[divan::bench]
    fn hybrid_fts_and_vector(bencher: divan::Bencher) {
        let index = SearchIndex::build(64, 24, true);
        let search = || divan::black_box(index.hybrid_search("sqlite vector index"));
        warm_up(&search);
        bencher.bench_local(search);
    }

    #[divan::bench]
    fn export_jsonl(bencher: divan::Bencher) {
        let index = SearchIndex::build(32, 24, false);
        let export = || divan::black_box(index.export_jsonl());
        warm_up(&export);
        bencher.bench_local(export);
    }
}

/// Usage dashboard aggregation and the per-message helpers around it.
mod analytics {
    use super::*;

    #[divan::bench(args = [1_000, 10_000])]
    fn aggregate_usage(bencher: divan::Bencher, events: usize) {
        let workload = UsageWorkload::generate(events);
        let aggregate = || divan::black_box(workload.aggregate());
        warm_up(&aggregate);
        bencher.bench(aggregate);
    }

    /// The instrumented region costs a few microseconds no matter what runs
    /// inside it. These two helpers take ~80 ns and ~226 ns per call, so a
    /// single call would report almost pure overhead — which is how a URL parse
    /// came to be measured at 31.6 us, and why its reported value moved by
    /// double digits without the code changing. Measure a batch instead, so the
    /// work dominates. The batch size is part of the benchmark name, so the
    /// reported figure is never mistaken for a per-call cost.
    const MICRO_BATCH: usize = 4_096;

    #[divan::bench(args = [MICRO_BATCH])]
    fn embedding_text(bencher: divan::Bencher, batch: usize) {
        let content = "\
            Recall keeps every local coding session in one SQLite index, then \
            layers FTS5 keyword search and sqlite-vec similarity on top of it.";
        let build = || {
            let mut bytes = 0;
            for _ in 0..batch {
                bytes +=
                    build_embedding_text(divan::black_box("Index a long session"), content).len();
            }
            divan::black_box(bytes)
        };
        warm_up(&build);
        bencher.bench(build);
    }

    #[divan::bench(args = [MICRO_BATCH])]
    fn remote_url_normalization(bencher: divan::Bencher, batch: usize) {
        let normalize = || {
            let mut matched = 0;
            for _ in 0..batch {
                matched +=
                    normalize_remote_url(divan::black_box("git@github.com:samzong/Recall.git"))
                        .map_or(0, |slug| slug.len());
            }
            divan::black_box(matched)
        };
        warm_up(&normalize);
        bencher.bench(normalize);
    }
}

/// Rendering a session for the CLI and for the shareable HTML page.
mod rendering {
    use super::*;

    #[divan::bench(args = [32, 256])]
    fn plain_transcript(bencher: divan::Bencher, messages: usize) {
        let workload = RenderWorkload::generate(messages);
        let render = || divan::black_box(workload.render_plain());
        warm_up(&render);
        bencher.bench(render);
    }

    #[divan::bench(args = [32, 256])]
    fn share_html(bencher: divan::Bencher, messages: usize) {
        let workload = RenderWorkload::generate(messages);
        let render = || divan::black_box(workload.render_html());
        warm_up(&render);
        bencher.bench(render);
    }
}
