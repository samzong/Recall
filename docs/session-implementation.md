# Session Implementation

## Backward Compatibility

- Existing command names and flags remain available: `recall search`,
  `recall export`, `recall import`, `recall usage`, `recall share init`, and the TUI.
- Existing `recall export` remains the bulk export command.
- Existing TUI shortcuts keep using the same internal session operations.
- Export record schema is `v7`: event records retain `files` and nullable
  `command_evidence_status` alongside native call identity and visibility.
  Import accepts `v2`–`v7`; older records default missing files to an empty list
  and scan status to null. These defaults mean unknown evidence. Pre-topology
  records retain `thread_role = null` with no parent links.
- `protocol_version` is `2`: the default scope of `recall search`,
  `recall session list`, and `recall export` now comes from the current
  directory. Scripts and extensions that relied on the flagless global scope
  must pass `--project all`.

## Implementation Notes

- Reuse existing source resolution from `resolve_source_filter`.
- Add store helpers for session lookup by `sessions.id` and by
  `(source, source_id)`.
- Keep the CLI command implementation in a dedicated `src/session.rs` module.
- Extend `export::ExportOptions` with explicit session ids so
  `session export --format jsonl` reuses the existing JSONL export path.
- Reuse `SearchEngine::hybrid_search` for `session list --query`.
- Reuse `share::publish_session` for `session share`.
- Inventory published pages from the managed publish directory; do not add a
  separate share registry or crawl `pages.dev`.
- Reuse the same Wrangler deploy path for `share unpublish` after deleting the
  local HTML file.
- Reuse `resume_command_for` and `app_command_for` for `session resume` and
  `session open`.
- Keep stdout clean for data output; send sync/share/deploy progress to stderr.

## Acceptance Criteria

- A coding agent can list candidate sessions with one JSON command.
- A coding agent can retrieve a full session transcript without opening the TUI.
- A coding agent can share a chosen session and receive the final URL as JSON.
- A coding agent can list currently published share URLs and unpublish one.
- A coding agent can export selected sessions without relying on search filters
  alone.
- A coding agent can resume or open supported sessions by id.
- `cargo test` covers argument parsing, session lookup, JSON output shape, and
  share dry-run behavior.
- Documentation includes at least one end-to-end agent workflow.

## Open Questions

- Should `session list --sync` support `--force`, or should forced sync stay only
  under `recall sync --force`?
- Should JSON `session show` return raw message content by default, sanitized
  content by default, or both?
- Should `session share` support custom one-off publish directories, or always
  use `recall share init` config?
- Should a future release add `session delete`, or should local deletion remain
  intentionally unsupported?
