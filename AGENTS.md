# AGENTS.md

This file provides guidance to AI coding agents (Claude Code, Codex, OpenCode,
etc.) when working with code in this repository.

## Overview

Recall is a Rust CLI/TUI application that indexes AI coding sessions from local
tools (Claude Code, Codex, OpenCode, Cursor, ...) into one SQLite database for
full-text/semantic search, usage tracking, JSONL export/import, session
sharing, and resume. It is an application crate, not a reusable Rust library.

## Commands

```bash
make check                            # full gate: fmt --check → clippy -D warnings → test
make build                            # debug build
make run                              # launch TUI
make sync                             # cargo run -- sync (FORCE=1 reprocesses all)
make search Q="query"                 # CLI search
cargo test <name>                     # run a single test by name filter
cargo test integration::regression    # regression suite
cargo test integration::eval_harness  # eval harness
```

`make check` must pass before push. CI runs exactly the same command — there is
no CI-only logic.

Releases use cargo-release: `make release-patch` is a dry run; add `EXECUTE=1`
to bump, commit, tag, and push. The `v*` tag triggers the GitHub Actions binary
build. One-time setup: `git config core.hooksPath .githooks` enables the DCO
signoff hook.

## Architecture

Data flow: source adapters → sync → SQLite → search → CLI/TUI.

- `src/adapters/` — one adapter per tool implements the `SourceAdapter` trait
  (`scan()` returns `RawSession`s) and is registered in `all_adapters()` in
  `src/adapters/mod.rs`. Registration alone wires the adapter into the DB
  schema, search, TUI source filter, and CLI `--source` flag. DEVELOPMENT.md
  documents the full adapter contract. Usage tracking is an optional extension
  on the same adapter — token events attached to the `RawSession` with a
  `parser_version` for backfill — never a separate adapter.
- `src/sync.rs` — incremental sync writes scanned sessions into SQLite.
- `src/db/` — rusqlite storage split by domain: sessions, events, projects,
  semantic index, skill audit, usage, schema, and search. Full-text search is
  SQLite FTS5; vector search is sqlite-vec.
- `src/embedding.rs`, `src/semantic.rs` — local embeddings via candle with
  `intfloat/multilingual-e5-small` (Metal on macOS, optional `cuda` feature).
- `src/tui/` — ratatui app: app state, event handling, layout, background
  search worker, share/usage/viewing state, and `ui/` rendering modules.
- `src/share/` — renders sessions to HTML and publishes share assets.
- `src/cli.rs` — clap subcommands dispatching to the modules above.
- `website/` — Next.js docs site (pnpm), independent of the Rust crate.

## Boundaries

- `src/lib.rs` exposes only `init()` and `run()`; `src/main.rs` only
  initializes tracing and calls them. Internal modules are `pub(crate)` by
  default — do not widen module, type, or function visibility unless a current
  in-repo caller requires it.
- `publish = false` in Cargo.toml is intentional: Recall ships binaries and
  Homebrew assets, not a crates.io package. Do not remove it or add public
  Rust API for external consumers unless the release strategy changes.
- Tests live in `src/integration/` inside the library crate, compiled through
  `#[cfg(test)] mod integration;`. Do not recreate standalone `tests/*.rs`
  test targets; `tests/fixtures/` holds fixture data only.
- Adapter rules (see DEVELOPMENT.md): return `Ok(vec![])` when a tool is not
  installed; open external databases read-only; extract text content only;
  `tracing::warn!` and skip on recoverable parse errors.
- `.local/` is local scratch and external comparisons, not an architecture
  source.
