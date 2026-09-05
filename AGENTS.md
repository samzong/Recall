# Recall

Rust CLI/TUI for indexing and searching local AI coding sessions.
Data flow: adapters -> sync -> SQLite -> search -> CLI/TUI.

Read the nested `AGENTS.md` before changing `src/adapters/`, `src/db/`,
`src/tui/`, `crates/rx/`, `extensions/`, or `website/`.
`CLAUDE.md` links to this file; edit this file once.

## Build and verify

- `make check` is required before push and is the CI gate: dependency audit,
  format check, workspace Clippy with `--all-targets --features bench`, then
  workspace tests. Install the cargo-audit version pinned in
  `.github/workflows/ci.yml`.
- `make build` builds core; `cargo build -p rx` or `cargo build -p recall-<name>`
  builds another workspace binary. `cargo test <filter>` runs focused core tests.
- Core integration tests belong in `src/integration/`, enabled by `src/lib.rs`;
  `tests/fixtures/` holds data. Do not add root `tests/*.rs` targets.
  Other workspace crates keep their own test layouts.
- Website commands and checks are separate; see `website/AGENTS.md`.

## Boundaries

- Core owns index writes, sync, import, storage, and migrations. Extensions
  consume the stable CLI JSON/JSONL protocol, never `recall.db` or Rust internals.
  See `docs/extensions.md` for the boundary and protocol contract.
- Machine-output commands emit only the requested data on stdout; progress and
  warnings go to stderr. Published fields cannot be removed, renamed, or change
  meaning without a `protocol_version` bump. SQLite schema is not public API.
- Keep Rust internals `pub(crate)` unless a current in-repo caller needs wider
  visibility. The normal library entrypoints are `init()` and `run()`.
  `publish = false` stays: workspace packages ship as application binaries.
- The optional `bench` feature exposes `src/bench_api.rs` for benchmarks only.
  Add fixtures there instead of widening internals; use temporary directories
  and in-memory databases, never the user's index.
- `crates/rx/` is an independent native harness launcher, with no Recall crate
  or database dependency. Read its design and admission rules through its
  nested `AGENTS.md`.
- `skills/recall/` is embedded in the core binary and installed by
  `recall skill install`; edits require rebuilding the binary.
- `.local/` is scratch, not an architecture source.

## Releases

- Setup and release commands are in `DEVELOPMENT.md` and `Makefile`; commit
  hooks live in `.githooks/` (`git config core.hooksPath .githooks`).
- Core uses cargo-release. The release owner chooses the version bump;
  `make release-patch` is a dry run, and `EXECUTE=1` bumps, commits, tags, and
  pushes. The git-cliff hook prepends the changelog; preserve existing entries.
- `.github/workflows/release.yml` owns tag validation and publication. Release
  artifacts must come from the same commit that passed the gate.
- Extensions release independently; version bumps declare release intent.
  See `extensions/AGENTS.md`. The generated
  `website/public/extensions/catalog.json` must not be hand-edited.
