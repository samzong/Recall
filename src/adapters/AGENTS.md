# src/adapters/

One adapter per AI coding tool. This is the most-extended surface in Recall —
DEVELOPMENT.md walks through adding an adapter; this file states the rules that
apply when working here.

## Contract

- The `SourceAdapter` trait in `mod.rs` is the authoritative contract, not the
  DEVELOPMENT.md example. `id()`, `label()`, `scan()`, and `resume_command()`
  are required; `scan_summary()`, `scan_for_sync()`, `prune()`,
  `app_command()`, and `usage_parser_version()` are optional overrides.
- Register new adapters in `all_adapters()` in `mod.rs`. Registration alone
  wires the adapter into sync, search, the TUI source filter, and the CLI
  `--source` flag. No schema change is needed — `sessions.source` is a value,
  not a column per tool.
- `events.rs`, `file_scan.rs`, and `sync_state.rs` are shared helpers, not
  adapters. Prefer `file_scan::run_file_scan_with_options` over hand-rolled
  mtime tracking for file-based sources.
- Usage and session events are extensions on the same `RawSession`
  (`with_usage`, `with_events`), each with its own parser version. Bump the
  parser version when parsing changes — that is what triggers backfill for
  sessions whose files did not change.
- `source_supports_event_backfill()` in `mod.rs` is a hardcoded source list.
  An adapter that starts emitting events must be added there, or usage
  dashboards will not pick it up.

## Rules

- Tool not installed → return `Ok(vec![])`, never an error.
- Open external tool databases read-only (`SQLITE_OPEN_READ_ONLY`).
- Extract text content only; skip tool calls, images, and internal metadata.
- Recoverable parse error → `tracing::warn!`, skip the session, continue.
- Timestamps are Unix milliseconds.

## Verify

```bash
make check
cargo run -- sync -v          # should log the new source with a session count
cargo run -- search "query" --source <id>
```
