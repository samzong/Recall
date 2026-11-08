# Session Sharing

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
