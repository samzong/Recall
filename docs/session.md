# Session CLI PRD

## Documents

- [Session sharing](session-sharing.md)
- [Session workflows](session-workflows.md)
- [Session scope](session-scope.md)
- [Session implementation](session-implementation.md)
- [File history](file-history.md)

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
matching only. Session search combines keyword and vector results when semantic
search is enabled and the required embeddings are available. Otherwise, it uses
keyword results only.

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

- `--query <text>`: use the same search path as `recall search`. When semantic
  search is enabled and embeddings are available, this path combines keyword
  and vector results. Otherwise, it uses keyword results only.
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
  "semantic_enabled": true,
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

`match_source` is `hybrid` only when keyword and vector results contribute to the
match. It is `fts` when keyword results alone contribute, or `vector` when only
vector results contribute. The example shows a hybrid match.

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
