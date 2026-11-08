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
`"semantic_enabled"` in `config.json` and defaults to `false`. A config file
without this field also defaults to `false`, so upgrades do not enable semantic
search.

```bash
recall semantic status
recall semantic disable
recall semantic enable
```

When semantic search is disabled, sync, import, browsing, export, and full-text
search keep working. Recall does not start an embedding worker or download or
load the model for indexing or search. Sessions still queue `pending` embedding
rows. `recall semantic enable` saves the setting and starts the background
indexer for queued sessions in the existing archive. It does not require a
reimport. If the model is not already available, enabling semantic search
requires a download of about 465 MiB.

Disabled semantic search uses full-text results without a notice on each CLI
search. `recall info` and `recall semantic status` report the setting and queue.
JSON responses for `recall search`, `recall session list --query`, and MCP
session search include a top-level `semantic_enabled` field. CLI list responses
without a nonempty query do not include this field.

If the config file is damaged or unreadable, CLI and MCP searches each write one
warning to stderr and continue with full-text search. Search does not change the
config file in this case. A requested sync still requires a valid config file.

In the TUI, press Ctrl+P to open Settings. The Semantic row toggles the setting
when you press Enter or Space. Saving applies the setting to the current TUI
immediately. Enabling it also starts the background indexer. A CLI
`recall semantic enable` or `disable` command does not update search in an
already running TUI. The TUI reads the setting at startup or when you next save
its settings. The background indexer rereads the setting at each session
boundary, so a running indexer applies an external CLI change at that boundary.

Benchmark commands bypass the semantic setting.

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
