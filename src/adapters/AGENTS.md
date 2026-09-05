# Adapters

`SourceAdapter` in `mod.rs` is the implementation contract;
`DEVELOPMENT.md` provides the walkthrough.

- Register each source in `all_adapters()`. This connects sync, search, and
  CLI/TUI source selection; adding a source needs no schema change.
- Implement `start_command()` when a native CLI supports an initial prompt.
  Handoff targets come from registered adapters with an available binary;
  do not create a separate target table.
- Adapters never mutate the Recall store. A complete source inventory may
  return a bounded `ReconcilePlan`; sync applies it only after successful
  processing and only in global scope.
- Missing source data returns an empty scan. Open external databases read-only.
- Extract message text separately from structured usage and session events;
  do not put tool payloads or images into message text.
- Warn on recoverable session parse failures and skip the affected session;
  use shared JSONL helpers to tolerate malformed individual records.
- Timestamps are Unix milliseconds. Reuse shared file-scan, JSON, timestamp,
  path, and event helpers.
- Usage and events belong to the same `RawSession`, with separate parser
  versions. Bump the relevant version when parsing changes to backfill
  unchanged files. Add new event sources to `source_supports_event_backfill()`.
