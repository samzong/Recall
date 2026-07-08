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
- `events.rs`, `file_scan.rs`, `sync_state.rs`, `json_util.rs`, and `paths.rs`
  are shared helpers, not adapters. Prefer them over hand-rolling:
  `file_scan::run_file_scan_with_options` for mtime tracking,
  `json_util::jsonl_indexed` for per-line JSONL read loops,
  `json_util::rfc3339_ms` and `json_util::json_i64` for timestamp/number
  coercion, `paths::resolve_home_dir` for `~/…` directory resolution, and
  `first_timestamp`/`last_timestamp` (`mod.rs`) for started_at/updated_at
  fallback chains.
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
