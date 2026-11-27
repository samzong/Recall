# Session Sharing

### `recall session share`

Publish one or more sessions to the configured share provider without opening the TUI.

```bash
recall session share --id <id>
recall session share --id <id> --include-related
recall session share --id <id-1> --id <id-2> --exclude-id <skip-id>
recall session share --source codex --source-id <source-id> --format json
recall session share --id <id> --dry-run
recall session share --id <id> --include-related --dry-run --output /tmp/share.html
recall session share --id <id> --open
recall session share --id <id> --copy-url
recall session share --id <id> --tldr-file /tmp/recall-tldr.md --format json
```

Options:

- `--id <session-id>`: Recall internal session UUID. Repeat this flag to include more sessions.
- `--source <source> --source-id <source-id>`: source-native lookup for one seed session.
- `--include-related`: climb to the tree root and include all non-empty related sessions.
- `--exclude-id <session-id>`: Recall session UUID to omit from the bundle. Repeat this flag to exclude more sessions.
- `--dry-run`: validate configuration, render size, target file path, and URL without deploying.
- `--output <path>`: write rendered HTML bytes to this path without touching the publish directory. Requires `--dry-run`.
- `--open`: open the resulting URL in the default browser.
- `--copy-url`: copy the resulting URL to the system clipboard.
- `--tldr-file <path>`: render this Markdown file as the top summary block. Skips missing, unreadable, or blank files.
- `--format <text|json>`: default `text`.

Behavior:

- Requires existing `recall share init` configuration for live publish or publish-directory preview.
- Rejects `--source` without `--source-id`, `--source-id` without `--source`, and mixing `--id` with `--source`.
- Uses the same Cloudflare Pages renderer and deployment path as the TUI.
- Enforces at most 64 sessions per bundle and 25 MiB per rendered HTML file.
- Single-session shares keep the 2-column layout and unchanged HTML structure.
- Multi-session bundles render a 3-column layout with a sticky session tree, active panel switching, and inline spawn cards.
- Forked subagents trim inherited leading messages when their parent session also exists in the bundle.
- Tool targets and legacy tool logs strip the session directory prefix and replace home paths with `~`.
- Re-publishing the same primary source session overwrites the deterministic route and reports added or removed sessions.
- Make sure that `project_domain` comes from configuration or Cloudflare Pages metadata.
- Prints progress to stderr and the final result to stdout.

JSON output:

```json
{
  "session": {
    "id": "4df8069c-1e42-48a9-80e5-0bcdd7dc6d9d",
    "source": "codex",
    "source_id": "019e6d8d-588b-7fd2-a326-c525469ed120",
    "title": "Fix bug"
  },
  "sessions": [
    {
      "id": "4df8069c-1e42-48a9-80e5-0bcdd7dc6d9d",
      "source": "codex",
      "source_id": "019e6d8d-588b-7fd2-a326-c525469ed120",
      "title": "Fix bug"
    }
  ],
  "share": {
    "provider": "cloudflare-pages",
    "project_name": "recall-share-7f3a2c",
    "project_domain": "recall-share-7f3a2c.pages.dev",
    "publish_dir": "/Users/me/Library/Application Support/recall/share-pages",
    "file_path": "/Users/me/Library/Application Support/recall/share-pages/019e6d8d-588b-7fd2-a326-c525469ed120.html",
    "share_id": "019e6d8d-588b-7fd2-a326-c525469ed120",
    "url": "https://recall-share-7f3a2c.pages.dev/019e6d8d-588b-7fd2-a326-c525469ed120",
    "html_bytes": 18432
  },
  "dry_run": false,
  "bundle_diff": {
    "added": [
      "claude-code:agent-123"
    ],
    "removed": []
  }
}
```

### Related session discovery (`recall session show --format json`)

`recall session show --format json` adds direct native children under `session.topology.children` and a top-level `related` array.

- `session.topology.children` always holds an array of direct spawn or subagent-fork children.
- `related` always holds an array of discovered tree descendants and Confer peer sessions.
- Each `related` item includes `id`, `source`, `source_id`, `parent_id`, `depth`, `relation`, `title`, `message_count`, and `started_at`.
- Confer peer items also include `via` (`"confer"`) and optional `role_label`.

### `recall share list`

List pages currently in the configured publish directory with their public URLs. This command reads the local inventory, not a live crawl of `pages.dev`.

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
- Reads title, source summary, and optional `session_count` from the rendered page.
- Prints an empty list when the publish directory does not exist.
- Refuses to list a directory that Recall does not manage.

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
      "source": "Codex + Claude · 2 sessions",
      "session_count": 2,
      "file_path": "/Users/me/Library/Application Support/recall/share-pages/019e6d8d-588b-7fd2-a326-c525469ed120.html",
      "html_bytes": 18432
    }
  ]
}
```

### `recall share unpublish`

Delete one published page from the local publish directory and redeploy so the public URL stops serving it. Alias: `recall share rm`.

```bash
recall share unpublish <share-id>
recall share unpublish https://recall-share-7f3a2c.pages.dev/<share-id>
recall share unpublish <share-id> --dry-run
recall share unpublish <share-id> --yes --format json
```

Options:

- `<share-id>`: the id from `recall share list`, or the published URL for this project. Other origins fail.
- `--dry-run`: resolve the page and print the URL without deleting or deploying.
- `--yes`: skip the interactive confirmation prompt. Required when stdin is not a terminal.
- `--format <text|json>`: default `text`.

Behavior:

- Requires existing `recall share init` configuration.
- Deletes `{share_id}.html` from the managed publish directory and deploys that directory with Wrangler.
- Restores the local HTML file if deploy fails.
- Keeps the Cloudflare Pages project intact.
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
