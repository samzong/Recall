# Recall

> Local-first search across every AI coding session on your machine.

[![Recall TUI](docs/recall.png)](https://asciinema.org/a/909453)

Jump between Claude Code, Codex, and whatever comes next; Recall pulls those scattered local sessions into one searchable index, tracks usage when token metadata is available, and drops you back into the original CLI.

## Install

```bash
brew install samzong/tap/recall
# or
make install # clone
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
| Gemini          |     ✅    |     ✅     |        ✅        |        ✅       |        ✅       |   ✅   |      |
| Kiro            |     ✅    |     ✅     |        ✅        |        ✅       |        ✅       |   —    |       |
| Copilot CLI     |     ✅    |     ✅     |        ✅        |        ✅       |        ✅       |   ✅   |      |
| Cursor          |     ✅    |     ✅     |        ✅        |        ✅       |        ✅       |   —    |       |
| Cline           |     ✅    |     ✅     |        ✅        |        ✅       |        ✅       |   —    |       |

## Usage

```bash
recall sync          # incremental sync (safe to run anytime)
recall sync --force  # reprocess every session (after changing embedding model)
recall               # launch TUI
recall search Q      # one-shot CLI search
recall search Q --project /path/to/repo
recall usage         # usage dashboard
recall usage --json  # usage report for scripts
recall export --jsonl --source codex --project /path/to/repo --limit 20
recall info          # index stats and worker status
```

## Export

`recall export --jsonl` writes one JSON object per indexed session. Each record
includes `schema_version`, `record_type`, `session`, `messages`, and
`usage_events`. Optional fields are emitted as `null`; raw source-specific JSON
is not part of the public export contract.

## License

[MIT](LICENSE)

## Acknowledgements

Thanks to [tokscale](https://github.com/junhoyeo/tokscale) for the usage dashboard reference and token accounting behavior.
