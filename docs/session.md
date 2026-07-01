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
- `--project <path>`: project directory boundary, including child paths.
- `--time <today|7d|week|30d|month|all>`: time window, default `all`.
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
      "match_source": "hybrid",
      "snippet": "wrangler pages deploy failed..."
    }
  ],
  "next_offset": null
}
```

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
    "is_import": false
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
Today this is expected to be useful for Codex desktop threads.

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
- `session show` should preserve Recall's existing sanitization behavior for
  displayed tool lines where applicable, but JSON output should clearly document
  whether content is sanitized or raw.
- `session share --dry-run` should be cheap and safe enough for agents to run
  before asking for final user approval.

## Backward Compatibility

- Existing commands keep working: `recall search`, `recall export`,
  `recall import`, `recall usage`, `recall share init`, and the TUI remain
  unchanged.
- Existing `recall export` remains the bulk export command.
- Existing TUI shortcuts keep using the same internal session operations.
- No existing JSONL export schema changes are required for the MVP.

## Implementation Notes

- Reuse existing source resolution from `resolve_source_filter`.
- Add store helpers for session lookup by `sessions.id` and by
  `(source, source_id)`.
- Keep the CLI command implementation in a dedicated `src/session.rs` module.
- Extend `export::ExportOptions` with explicit session ids so
  `session export --format jsonl` reuses the existing JSONL export path.
- Reuse `SearchEngine::hybrid_search` for `session list --query`.
- Reuse `share::publish_session` for `session share`.
- Reuse `resume_command_for` and `app_command_for` for `session resume` and
  `session open`.
- Keep stdout clean for data output; send sync/share/deploy progress to stderr.

## Acceptance Criteria

- A coding agent can list candidate sessions with one JSON command.
- A coding agent can retrieve a full session transcript without opening the TUI.
- A coding agent can share a chosen session and receive the final URL as JSON.
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
