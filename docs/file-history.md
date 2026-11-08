# File History

## File History Implementation Contract

Use MCP `file_history` to find recorded operations on a target file across
session projects. This reads the index without syncing or executing history.
Pass an explicit `target_project` and an exact repository-relative or absolute
`path`; omit `project`, which retains its older session-scope meaning.

```json
{"target_project":"owner/repo","path":"src/main.rs","include_command_candidates":true,"limit":20}
```

`target_project` accepts a local directory, remote URL, or unique indexed target
repository name/slug. Repository identity can match across worktrees, including
sessions started elsewhere. Check returned `target_file` and each match's
`match_basis`. Ambiguous selectors require a more specific target. Basename or
suffix matching belongs to the legacy mode without `target_project`.

If a historical worktree is gone and its repository identity is unresolved,
a path-only legacy query can discover recorded absolute paths. Retry target
mode with that exact native path to read its evidence. Inspect `match_basis`;
a suffix match alone does not establish repository identity. Legacy discovery
is limited to 50 events and does not establish complete coverage.

### Interpret and page file evidence

Structured target mode includes all event kinds by default, with command
candidates excluded unless `include_command_candidates` is true. An explicit
`kind` filters the event kind. The default page holds 20 events, at most 50.
Repeat the same target, path, source, kind, and candidate selection with
`next_cursor` until `has_more` is false. Target-relevant index changes invalidate
the cursor; restart the query rather than joining incompatible pages. Known
timestamps sort newest first, with unknown timestamps last.

Each page checks the count and highest ID of matching immutable indexed events.
Reparsing replaces events with new IDs, invalidating affected continuations.
This uses indexed target associations without reading full evidence payloads.
Selected page metadata and file associations share a 64 MiB read budget.

File associations distinguish `call`, `observation`, and `command`, and retain
operations such as read, write, delete, and both sides of a move. Requests,
results, and observations may describe one operation. Command evidence is a
candidate; approval, wrapper completion, or a filename in output does not prove
execution or success. `command_evidence_status` describes scanning coverage:
`complete`, `unsupported`, `limit_exceeded`, or null for unscanned/older records.
It does not describe execution success. Preserve native result statuses and
use native call identity and surrounding evidence before combining records.
Event rows, equal content, and Git commits are not counts of independent edits.

Retain `coverage` from the first page; continuation pages omit that field.
Check it and per-hit truncation flags before reporting completeness. Coverage
describes all indexed sessions of the selected sources, not the
history of this file alone. It reports recorded parser versions, imports,
and missing parser state. Per-hit evidence reports file identity and command
scan status separately.
It does not scan native sources or prove that parsers are current. Empty
matches do not prove that the file was never changed.

### Read evidence and the discussion separately

Copy `events[].evidence.event_ref` and the hit's `session_id` into MCP
`get_session`:

```json
{"session_id":"<session-id>","event_ref":"<opaque-event-ref>","evidence_part":"payload","max_bytes":16384}
```

Concatenate each page's UTF-8 `data` in byte-offset order before parsing the
payload JSON. Continue with the same `session_id`, `event_ref`, `evidence_part`,
and the returned `next_cursor` as `cursor`. `max_bytes` bounds each evidence
response, defaults to 16,384, and accepts 1,024–65,536. Each read has a 64 MiB
budget; oversized evidence fails explicitly instead of returning a complete
prefix. References identify an immutable event within one index. Any rebuild
of that event invalidates its reference, even if its content is identical;
query again for a fresh reference. Evidence cursors also bind the content
digest and reject native content changes.

The payload contains the full indexed event, native `attrs_json`, all file
associations, same-session `related_event_refs`, and an optional `discussion`
selector. Read related payloads to connect a request to its native result.
For Cursor content, select the result reference whose native attrs contain
`beforeContentId` or `afterContentId`, then request `evidence_part: "before"`
or `"after"`. A call without those references returns
`content_reference_not_recorded`; it is not evidence that the source changed.
Native content is read only through the registered Cursor store, with session
ownership and content-hash checks. Imports and unverifiable references remain
`source_unverified`; unavailable or changed source records are reported as
`source_missing` or `source_changed`. Indexed payload remains readable without
claiming that the native source is still present or verified.

Use the returned `discussion` object in a separate `get_session` call, without
`event_ref`. It uses the existing `around_seq` message window and continuation
rules. A missing anchor remains unknown; do not infer it from event order.
Explain the reason for an edit only when the recorded discussion supports it,
and separate the user's request, the agent's explanation, and an inference.

### Refresh the index

When index mutation is authorized, backfill native events across configured
sources with an explicit global scope to include cross-project operations:

```bash
recall sync --backfill-events --project all --dry-run
recall sync --backfill-events --project all
```

Backfill bypasses source time windows while respecting enabled sources,
exclusions, `--source`, and session project scope. It updates events in existing
sessions without rebuilding their discussion, usage, or embeddings. Newly
encountered sessions with events use normal initial indexing, including
discussion, parent relationships, usage and background embedding scheduling. If parsed discussion differs from an existing indexed transcript,
unverifiable anchors are cleared. Use normal `recall sync --project all` to
refresh supported discussion parsers; normal sync retains its usual scope,
time-window, and retention behavior.

`--dry-run` requires `--backfill-events` and leaves the index unchanged. It does
not migrate an older database; `requires_index_upgrade` means a normal writable
index upgrade is required before previewing. Inspect the maintenance report
for missing or unknown originals, unsupported sessions, unstable reads, and
failures. Backfill does not prune sessions or reconcile deletions. It cannot
recover records the native source deleted or never stored, and does not add
permanent archival, a filesystem monitor, a launcher, or a TUI flow.

File evidence preserves the native `path` and optional operation `cwd`.
Its `target` is derived during sync from available repository evidence,
independently of the session's project. Missing files may resolve through an
existing parent directory; unresolved paths remain unresolved. Derived identity
does not prove historical Git state. Import preserves recorded targets without
resolving imported paths on disk.
