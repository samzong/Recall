# Recall

> Local-first search across every AI coding session on your machine.

[![Recall TUI](docs/recall.png)](https://asciinema.org/a/909453)

Jump between Claude Code, Codex, and whatever comes next; Recall pulls those scattered local sessions into one searchable index, tracks usage when token metadata is available, and drops you back into the original CLI.

## Install

```bash
brew install samzong/tap/recall
# or
make install # from a source checkout
```

## Support

One index across every AI coding CLI. Sync once, search everywhere, resume right where you left off.

| Adapter         | Discovery | Full-index | Incremental-sync | Semantic-search | Export | Resume | Usage |
| --------------- | :-------: | :--------: | :--------------: | :-------------: | :-------------: | :----: | :----: |
| Claude Code     |     ✅    |     ✅     |        ✅        |        ✅       |        ✅       |   ✅   |   ✅   |
| OpenCode        |     ✅    |     ✅     |        ✅        |        ✅       |        ✅       |   ✅   |   ✅   |
| Codex           |     ✅    |     ✅     |        ✅        |        ✅       |        ✅       |   ✅   |   ✅   |
| Pi              |     ✅    |     ✅     |        ✅        |        ✅       |        ✅       |   ✅   |   ✅   |
| Antigravity CLI |     ✅    |     ✅     |        ✅        |        ✅       |        ✅       |   ✅   |      |
| Gemini          |     ✅    |     ✅     |        ✅        |        ✅       |        ✅       |   ✅   |   ✅   |
| Kiro            |     ✅    |     ✅     |        ✅        |        ✅       |        ✅       |   —    |       |
| Copilot CLI     |     ✅    |     ✅     |        ✅        |        ✅       |        ✅       |   ✅   |      |
| Cursor          |     ✅    |     ✅     |        ✅        |        ✅       |        ✅       |   —    |   ✅   |
| Cline           |     ✅    |     ✅     |        ✅        |        ✅       |        ✅       |   —    |       |
| Grok            |     ✅    |     ✅     |        ✅        |        ✅       |        ✅       |   ✅   |        |

## Usage

```bash
recall sync          # incremental sync (safe to run anytime)
recall sync --force  # reprocess every session (after changing embedding model)
recall               # launch TUI
recall search Q      # one-shot CLI search
recall search Q --project /path/to/repo
recall usage         # usage dashboard
recall usage --json  # usage report for scripts
recall export --source codex --project /path/to/repo --limit 20 > recall-export.jsonl
recall import recall-export.jsonl --dry-run  # preview an import
recall session list --source codex --limit 20  # list sessions for agents/scripts
recall session show --id <session-id> --include metadata,messages --format json
recall session share --id <session-id> --format json  # publish one selected session
recall info          # index stats and worker status
```

## Export

`recall export` writes JSON Lines to stdout, with one JSON object per indexed
session. Redirect stdout to save an export file:

```bash
recall export --source codex --project /path/to/repo > recall-export.jsonl
```

Each session includes `schema_version`, `record_type`, `session`, `messages`,
`usage_events`, and `events`. It covers the portable session data Recall can
import again; derived index state such as FTS rows, embeddings, and background
job state is rebuilt locally. Optional fields are emitted as `null`. By default
all sessions are exported; use `--limit N` to truncate. `--time` filters on
`started_at`, so prefer a full export when moving data between machines.

## Import

`recall import <file>` (or `-` for stdin) loads sessions from an export file
into the local index. How the file travels between machines is up to you.

```bash
# machine A
recall export > recall-a.jsonl
# machine B
recall import recall-a.jsonl
```

- Idempotent: a session whose `(source, source_id)` already exists locally is
  skipped, so re-running an import is safe and local data always wins.
- Imported sessions are searchable and appear in usage reports, but cannot be
  resumed on this machine (the source tool's own files were not copied); the
  TUI explains this if you try.
- `--dry-run` parses and reports counts without writing anything.

## License

[MIT](LICENSE)

## Acknowledgements

Thanks to [tokscale](https://github.com/junhoyeo/tokscale) for the usage dashboard reference and token accounting behavior.
