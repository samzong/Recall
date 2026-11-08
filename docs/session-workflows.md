# Session Workflows

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

### `recall semantic`

Manage semantic indexing and search. The setting persists as
`"semantic_enabled"` in `config.json` and defaults to `true`.

```bash
recall semantic status
recall semantic disable
recall semantic enable
```

While disabled, sync, import, browsing, export, and full-text search keep
working; no embedding worker is spawned and neither indexing nor search
downloads or loads the model. Sessions still queue `pending` embedding rows,
so `recall semantic enable` (or any later sync) resumes indexing over the
existing archive without a reimport.

`recall info` reports the setting under Settings and Semantic Queue, and JSON
session search responses carry a top-level `semantic_enabled` field; MCP
session search responses carry the same field. Text searches print a stderr
note that results are full-text only.

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
