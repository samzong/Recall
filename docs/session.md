# Session CLI PRD

## Goal

Make every session operation available through a non-interactive CLI so
coding agents can discover local sessions, present a short candidate list to the
user, and then act on the user's chosen session without driving the TUI.

The primary workflow is:

1. An agent runs a CLI command to list or search local sessions.
2. The agent shows the user a concise set of candidates.
3. The user chooses which session is safe to act on.
4. The agent shares, exports, resumes, or opens that exact session from the CLI.

## Problem

Recall already supports indexing, searching, usage reporting, import/export, and
Cloudflare Pages sharing. However, several session-level actions are available
only from the TUI:

- selecting a single session from search results;
- viewing the full message transcript;
- exporting the currently viewed session as text;
- sharing the currently viewed session to Cloudflare Pages;
- resuming or opening the selected session in its source tool.

That makes automation brittle. A coding agent has to start the TUI, send
keystrokes, parse terminal UI output, and hope the focused row did not change.

## Users

- Coding agents that need reliable, scriptable access to local session history.
- Power users who want shell-native workflows for Recall sessions.
- Maintainers who need stable command surfaces for tests and documentation.

## Definitions

- **Session**: one indexed Recall session row plus its messages and related usage
  or event metadata.
- **Session ID**: Recall's internal stable UUID stored in `sessions.id`.
- **Source ID**: the source tool's native session identifier, stored with
  `sessions.source` and `sessions.source_id`.
- **Session reference**: any unambiguous way to identify a session:
  `--id <session-id>` or `--source <source> --source-id <source-id>`.

## Scope

### In Scope

- Add a `recall session` command group.
- List sessions from the local Recall index with source, project, time, query,
  sort, and pagination filters.
- Show one session's metadata, messages, usage events, and session events.
- Export one or more explicitly selected sessions.
- Share one explicitly selected session to the configured Cloudflare Pages target.
- Resume or open one explicitly selected session when the source adapter supports
  it.
- Provide stable JSON and JSONL output for automation.
- Keep human-readable table/text output as the default for people.

### Out of Scope

- Remote auth or private access control for shared pages.
- Remote share revocation or deployment cleanup.
- Share provider abstraction beyond the current Cloudflare Pages target.
- Editing source-tool session data.
- Deleting local Recall sessions in the first release.
- Replacing the TUI.

## Command Design

### `recall search --messages`

Search individual message text with FTS, including assistant responses. The
existing `recall search` mode continues to return sessions.

```bash
recall search "sync lock" --messages --project owner/repo --limit 10 --format json
recall search "sync lock" --messages --session-id <session-id> --format json
```

`--limit` defaults to 10 and accepts 1–50 message matches. `--session-id`
limits the search to an exact indexed session; it does not infer a project
from the working directory. Explicit source, time, project, and repository
filters still apply. JSON contains `protocol_version` and `matches`; each match
has `session_id`, `source_session_id`, `source`, `title`, `seq`, `role`,
`timestamp` (Unix milliseconds or null), and a match-centered `excerpt`.
Matches are ranked and bounded, not an exhaustive list. This mode uses keyword
matching only; existing session search retains hybrid retrieval.

### `recall session list`

List indexed sessions. This command reads the local Recall SQLite index; it does
not scan source tools unless `--sync` is passed.

```bash
recall session list
recall session list --source codex --project /path/to/repo --time 7d
recall session list --query "cloudflare api token" --limit 20 --format json
recall session list --all --format jsonl
recall session list --sync --source codex --time today
```

Options:

- `--query <text>`: run the same hybrid search path as `recall search`.
- `--source <source>`: source id or label, matching existing source filters.
- `--project <selector>`: scope selector. A path is a directory boundary
  including child paths; `owner/repo` or a remote URL is a repository identity
  spanning every worktree; `all` is every project. Without this flag the scope
  is derived from the current directory (see Project Scope).
- `--time <today|7d|week|30d|month|all>`: time window, default `all`.
- `--thread-role <primary|subagent|unknown>`: filter by topology role. `unknown`
  selects sessions the source could not classify.
- `--limit <n>`: maximum sessions to return, default `50`.
- `--offset <n>`: skip sessions for pagination, default `0`.
- `--sort <newest|oldest|updated|relevance>`: default `newest`, or `relevance`
  when `--query` is set.
- `--all`: return all matching sessions; mutually exclusive with `--limit`.
- `--sync`: run an incremental sync before listing.
- `--format <table|json|jsonl>`: default `table`.

JSON output:

```json
{
  "filters": {
    "query": "cloudflare api token",
    "source": "codex",
    "project": "/path/to/repo",
    "time": "7d",
    "thread_role": "primary",
    "limit": 20,
    "offset": 0,
    "sort": "relevance"
  },
  "sessions": [
    {
      "id": "4df8069c-1e42-48a9-80e5-0bcdd7dc6d9d",
      "source": "codex",
      "source_label": "CDX",
      "source_id": "019e6d8d-588b-7fd2-a326-c525469ed120",
      "title": "Fix Cloudflare Pages deploy token handling",
      "project": "/path/to/repo",
      "started_at": 1781234567890,
      "updated_at": 1781235567890,
      "message_count": 42,
      "is_import": false,
      "topology": { "thread_role": "primary", "parents": [] },
      "match_source": "hybrid",
      "snippet": "wrangler pages deploy failed..."
    }
  ],
  "next_offset": null
}
```

`topology` is always present. `thread_role` is `primary`, `subagent`, or `null`
when the source provides no reliable classification. `parents` lists portable
parent links `{ "relation": "spawn|fork|resume", "source": ..., "source_id": ... }`;
a parent may not be indexed locally. A `fork` relation alone never implies a
subagent role.

### `recall session show`

Show one session. By default, print readable metadata and transcript text.

```bash
recall session show --id 4df8069c-1e42-48a9-80e5-0bcdd7dc6d9d
recall session show --source codex --source-id 019e6d8d-588b-7fd2-a326-c525469ed120
recall session show --id <id> --messages --format json
recall session show --id <id> --include usage,events --format json
```

Options:

- `--id <session-id>`: Recall internal session UUID.
- `--source <source> --source-id <source-id>`: source-native lookup.
- `--messages`: include messages; default true for text, false for JSON unless
  explicitly requested.
- `--include <metadata,messages,usage,events>`: comma-separated detail set.
- `--from-seq <n>` and `--to-seq <n>`: restrict message sequence range.
- `--role <user|assistant|all>`: message role filter, default `all`.
- `--format <text|json|jsonl>`: default `text`.

JSON output:

```json
{
  "session": {
    "id": "4df8069c-1e42-48a9-80e5-0bcdd7dc6d9d",
    "source": "codex",
    "source_id": "019e6d8d-588b-7fd2-a326-c525469ed120",
    "title": "Fix Cloudflare Pages deploy token handling",
    "project": "/path/to/repo",
    "started_at": 1781234567890,
    "updated_at": 1781235567890,
    "message_count": 42,
    "is_import": false,
    "topology": {
      "thread_role": "subagent",
      "parents": [
        { "relation": "spawn", "source": "codex", "source_id": "019e6d8d-parent" }
      ]
    }
  },
  "messages": [
    {
      "seq": 0,
      "role": "user",
      "timestamp": 1781234567890,
      "content": "Why did sharing fail?"
    }
  ],
  "usage_events": [],
  "events": []
}
```

Read around a search hit:

```bash
recall session show --id <session-id> --messages --around-seq 83 --before 3 --after 3 --format json
recall session show --id <session-id> --messages --from-seq 80 --to-seq 86 --max-chars 6000 --format json
recall session show --id <session-id> --messages --cursor '<next_cursor>' --format json
```

`--around-seq` selects the exact anchor plus actual neighboring messages,
including across gaps in sequence numbers. `--before` and `--after` default to
3, allow zero, and require `--around-seq`. Around and explicit range selectors
are mutually exclusive. Missing or ambiguous anchors are errors. The role
filter is applied after selecting the neighboring window.

Around reads default to 6,000 Unicode content characters per page. Set
`--max-chars` to 1–32,000 to change the budget or enable range paging. Pages
contain at most 1,000 message fragments and may end inside a message. Selected
messages are returned in conversation order, so a long preceding message can
fill a page before the anchor; use zero neighbors to read only the anchor.
Paged JSON/JSONL adds `truncated`, `next_cursor`, and
`first_message_byte_offset` (UTF-8 offset in the first returned message).
Text mode prints the continuation argument on stderr. Continue with the same
session reference and `--cursor`, without selection or role flags. Reindexing
the session invalidates the cursor even if its content is unchanged. Sequence
anchors refer to the current index, not permanent source identities.

Without around, cursor, or a character budget, CLI show retains its existing
full-content output. Bounds and role filtering are applied in SQLite.

MCP exposes the same message search as `search_messages`. Pass `around_seq`,
`before`, and `after` to `get_session`, or use `from_seq` / `to_seq`. Selected
MCP reads default to at most 50 messages and 6,000 content characters;
`max_chars` may lower this budget. `next_cursor` continues the selected window.
Selectors and cursor cannot be combined with `tail`. Legacy `get_session`
head/tail calls retain their existing text format and limits.

### `recall session export`

Export explicitly selected sessions. This complements the existing bulk
`recall export` command, which is filter-oriented.

```bash
recall session export --id <id> --output session.jsonl
recall session export --source codex --source-id <source-id> --format jsonl
recall session export --ids-file selected-sessions.txt --output selected.jsonl
recall session export --id <id> --format text --output session.txt
```

Options:

- `--id <session-id>`: may be repeated.
- `--source <source> --source-id <source-id>`: export one source-native session.
- `--ids-file <path>`: newline-delimited session ids.
- `--format <jsonl|text>`: default `jsonl`.
- `--output <path>`: write to file; stdout if omitted.

### `recall session share`

Publish one selected session to the configured share provider without opening the
TUI.

```bash
recall session share --id <id>
recall session share --source codex --source-id <source-id> --format json
recall session share --id <id> --dry-run
recall session share --id <id> --open
recall session share --id <id> --copy-url
recall session share --id <id> --tldr-file /tmp/recall-tldr.md --format json
```

Options:

- `--id <session-id>`: Recall internal session UUID.
- `--source <source> --source-id <source-id>`: source-native lookup.
- `--dry-run`: validate config, render size, target file path, and URL, but do
  not deploy.
- `--open`: open the resulting URL in the default browser.
- `--copy-url`: copy the resulting URL to the system clipboard.
- `--tldr-file <path>`: render this markdown file as the TL;DR block at the
  top of the shared page. Missing, unreadable, or blank files are skipped.
- `--format <text|json>`: default `text`.

Behavior:

- Requires existing `recall share init` configuration.
- Uses the same Cloudflare Pages renderer and deployment path as the TUI. The
  supported provider is Cloudflare Pages on `pages.dev`.
- Writes one static HTML file to the configured publish directory and deploys
  that directory with Wrangler.
- Re-publishing the same source session overwrites the same deterministic route.
- Renders a TL;DR block above the transcript only when a readable non-blank
  `--tldr-file` is supplied.
- TUI shares do not pass `--tldr-file`, so they keep the plain transcript page.
- The page shows readable user and assistant messages, collapses tool calls and
  tool results by default, and must not show local filesystem paths.
- Returns a deterministic URL for the selected source session.
- Uses the actual `project_domain` stored or resolved from Cloudflare Pages
  project metadata; it must not guess `project_name.pages.dev` when the domain
  is missing.
- Fails before deploy if the rendered page exceeds the Cloudflare Pages asset
  limit.
- Prints progress to stderr and the final result to stdout.

JSON output:

```json
{
  "session": {
    "id": "4df8069c-1e42-48a9-80e5-0bcdd7dc6d9d",
    "source": "codex",
    "source_id": "019e6d8d-588b-7fd2-a326-c525469ed120"
  },
  "share": {
    "provider": "cloudflare-pages",
    "project_name": "recall-share-7f3a2c",
    "project_domain": "recall-share-7f3a2c.pages.dev",
    "share_id": "019e6d8d-588b-7fd2-a326-c525469ed120",
    "url": "https://recall-share-7f3a2c.pages.dev/019e6d8d-588b-7fd2-a326-c525469ed120"
  },
  "dry_run": false
}
```

### `recall share list`

List pages currently in the configured publish directory, with their public
URLs. This is the local inventory that the next Cloudflare Pages deploy
publishes; it is not a live crawl of `pages.dev`.

```bash
recall share list
recall share list --format json
```

Options:

- `--format <text|json>`: default `text`.

Behavior:

- Requires existing `recall share init` configuration.
- Reads `*.html` files from the managed publish directory.
- Reconstructs each URL as `https://{project_domain}/{share_id}`.
- Title and source come from the rendered page when present.
- A missing publish directory prints an empty list, not an error.
- Refuses to list a directory that is not managed by Recall.

JSON output:

```json
{
  "provider": "cloudflare-pages",
  "project_name": "recall-share-7f3a2c",
  "project_domain": "recall-share-7f3a2c.pages.dev",
  "publish_dir": "/Users/me/Library/Application Support/recall/share-pages",
  "url_base": "https://recall-share-7f3a2c.pages.dev",
  "shares": [
    {
      "share_id": "019e6d8d-588b-7fd2-a326-c525469ed120",
      "url": "https://recall-share-7f3a2c.pages.dev/019e6d8d-588b-7fd2-a326-c525469ed120",
      "title": "Fix bug",
      "source": "Codex",
      "file_path": "/Users/me/Library/Application Support/recall/share-pages/019e6d8d-588b-7fd2-a326-c525469ed120.html",
      "html_bytes": 18432
    }
  ]
}
```

### `recall share unpublish`

Delete one published page from the local publish directory and redeploy so the
public URL stops serving it. Alias: `recall share rm`.

```bash
recall share unpublish <share-id>
recall share unpublish https://recall-share-7f3a2c.pages.dev/<share-id>
recall share unpublish <share-id> --dry-run
recall share unpublish <share-id> --yes --format json
```

Options:

- `<share-id>`: the id from `recall share list`, or this project's published
  URL (`https://{project_domain}/{share-id}`). Other origins are rejected.
- `--dry-run`: resolve the page and print the URL without deleting or deploying.
- `--yes`: skip the interactive confirmation prompt. Required when stdin is not
  a terminal.
- `--format <text|json>`: default `text`.

Behavior:

- Requires existing `recall share init` configuration.
- Deletes `{share_id}.html` from the managed publish directory, then deploys
  that directory with Wrangler. Cloudflare Pages deployments are full snapshots,
  so the public route 404s after a successful deploy.
- If deploy fails, the local HTML file is restored.
- Does not delete the Cloudflare Pages project.
- Prints progress to stderr and the URL to stdout.

JSON output:

```json
{
  "share": {
    "share_id": "019e6d8d-588b-7fd2-a326-c525469ed120",
    "url": "https://recall-share-7f3a2c.pages.dev/019e6d8d-588b-7fd2-a326-c525469ed120",
    "title": "Fix bug",
    "source": "Codex",
    "file_path": "/Users/me/Library/Application Support/recall/share-pages/019e6d8d-588b-7fd2-a326-c525469ed120.html",
    "html_bytes": 18432
  },
  "dry_run": false
}
```

### `recall session resume`

Resume one selected session in the source CLI when the adapter supports it.

```bash
recall session resume --id <id>
recall session resume --source claude-code --source-id <source-id>
recall session resume --id <id> --print-command
```

Options:

- `--id <session-id>` or `--source <source> --source-id <source-id>`.
- `--print-command`: print the command instead of executing it.
- `--format <text|json>`: default `text`.

If the source does not support resume, exit non-zero with an actionable error.

### `recall session open`

Open a selected session in its source app when an adapter supports app-open.
Today this is expected to be useful for Codex desktop threads
(`codex://threads/<id>`) and GitHub Copilot desktop sessions
(`ghapp://sessions/<id>`).

```bash
recall session open --id <id>
recall session open --id <id> --print-command
```

Options mirror `session resume`.

## Agent-Friendly Workflows

### Share A User-Selected Codex Session

```bash
recall sync --source codex
recall session list --source codex --project /path/to/repo --time 7d --format json --limit 10
# Agent asks user which session to share.
recall session share --id <chosen-id> --format json
```

### Inspect Before Sharing

```bash
recall session show --id <chosen-id> --include metadata,messages --format text
# User confirms the transcript is safe.
recall session share --id <chosen-id> --format json
```

### List Or Take Down Published Shares

```bash
recall share list --format json
# User chooses which public URL to remove.
recall share unpublish <share-id> --yes --format json
```

### Export Selected Candidates

```bash
recall session list --query "db migration failure" --format json --limit 5
recall session export --id <id-1> --id <id-2> --output migration-sessions.jsonl
```

## Error Handling

Every command must:

- return exit code `0` only on success;
- return exit code `2` for invalid CLI arguments;
- return exit code `3` for a session lookup miss;
- return exit code `4` for unsupported source actions such as resume/open;
- return exit code `5` for share provider or deploy failures;
- write human-readable errors to stderr;
- write machine-readable errors when `--format json` is selected.

Example JSON error:

```json
{
  "error": {
    "code": "session_not_found",
    "message": "No session matched source=codex source_id=missing",
    "hint": "Run recall session list --source codex --format json"
  }
}
```

## Privacy And Safety

- Sharing remains public to anyone with the URL.
- Recall sets no-index headers and robots rules for shared pages, but this is
  not access control.
- Auth is not supported now; if needed later, it belongs in a separate
  Cloudflare-backed design.
- `session share` must not add automatic confirmation prompts; coding agents
  should ask the user before invoking it.
- `share unpublish` prompts on a TTY and requires `--yes` when stdin is not a
  terminal; coding agents should ask the user before passing `--yes`.
- `session show` should preserve Recall's existing sanitization behavior for
  displayed tool lines where applicable, but JSON output should clearly document
  whether content is sanitized or raw.
- `session share --dry-run` should be cheap and safe enough for agents to run
  before asking for final user approval.

## Session Topology

Session topology records provenance without title/directory/time heuristics. It
appears in `session list`/`show` JSON and JSONL export under `session.topology`,
and both `recall session list` and bulk `recall export` accept
`--thread-role <primary|subagent|unknown>`.

Model (source-neutral):

- `thread_role`: `primary` (top-level user-owned execution), `subagent`, or
  `null` when the source gives no reliable classification. `primary` does not
  mean the session has no fork history.
- `parents[]`: portable `{ relation, source, source_id }` links. `relation` is
  `spawn`, `fork`, or `resume`. A session may have several parents. Missing
  parent sessions are valid — the unresolved portable identity is still exported.
  A `fork` relation alone is never proof of a subagent.

Per-adapter coverage:

| Source | Role | Parent links | Source signal |
| --- | --- | --- | --- |
| Codex | primary / subagent | spawn + fork | `session_meta.thread_source` / `source`, `parent_thread_id` / `thread_spawn`, `forked_from_id` |
| Claude Code | primary / subagent | spawn | `subagents/<agent>.jsonl` path + transcript `sessionId` parent |
| Pi | primary | fork | session-header `parentSession` |
| Others (OpenCode, Cursor, Copilot, Gemini, Grok, Antigravity, Cline, Kiro) | `null` | none | not yet classified |

Grok retains its current skip-and-prune behavior; enabling classified Grok
ingestion is a separate search/TUI visibility review before the path is turned
on.

## Project Scope

Commands that operate on a set of sessions — `recall search`, `recall session
list`, `recall export` — resolve their scope in this order:

1. explicit `--project <selector>`;
2. otherwise the current directory: a Git checkout with a resolvable `origin`
   means that repository identity (so sibling worktrees are included), a Git
   checkout without one means the top-level directory, and anything else means
   every project.

`recall sync` and `recall session list --sync` use the same rule, and a
`--sync` listing always syncs exactly the scope it then shows.

An inferred non-global scope is reported on stderr, and every structured
response carries `filters.effective_scope` with `kind`
(`repository|directory|global`) and `value`. Pass `--project all` for the
explicit global scope; `--repo` is deprecated in favour of `--project`.

A scoped sync never writes, refreshes, or deletes a session outside its scope.
Two maintenance actions are therefore global-only and run on
`recall sync --project all` (or the background worker started by the TUI):
pruning sessions whose source transcript disappeared, and backfilling repo
identity for sessions indexed before it was resolvable. Deleting sessions that
match `excluded_paths` still runs, restricted to the current scope.

## Backward Compatibility

- Existing commands keep working: `recall search`, `recall export`,
  `recall import`, `recall usage`, `recall share init`, and the TUI remain
  unchanged.
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

## Open Questions

- Should `session list --sync` support `--force`, or should forced sync stay only
  under `recall sync --force`?
- Should JSON `session show` return raw message content by default, sanitized
  content by default, or both?
- Should `session share` support custom one-off publish directories, or always
  use `recall share init` config?
- Should a future release add `session delete`, or should local deletion remain
  intentionally unsupported?
