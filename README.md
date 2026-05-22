# Recall

> Local-first search across every AI coding session on your machine.

[![Recall TUI](recall.png)](https://asciinema.org/a/909453)

You bounce between Claude Code, Codex, Copilot CLI, Cline, and whatever comes next. Each tool keeps its own sessions in its own place, in its own format. Recall pulls them all into one local index you can actually search — and drops you right back into any session in its original CLI.

## Architecture

![Recall Architecture](docs/architecture.png)

## Install

```bash
brew install samzong/tap/recall
# or
make install # clone
```

## Support

One index across every AI coding CLI. Sync once, search everywhere, resume right where you left off.

| Adapter         | Discovery | Full index | Incremental sync | Keyword search | Semantic search | Source filter | Time filter | Session search | Copy message | Markdown export | Resume |
| --------------- | :-------: | :--------: | :--------------: | :------------: | :-------------: | :-----------: | :---------: | :------------: | :----------: | :-------------: | :----: |
| Claude Code     |     ✅    |     ✅     |        ✅        |       ✅       |        ✅       |       ✅      |      ✅     |       ✅       |      ✅      |        ✅       |   ✅   |
| OpenCode        |     ✅    |     ✅     |        ✅        |       ✅       |        ✅       |       ✅      |      ✅     |       ✅       |      ✅      |        ✅       |   ✅   |
| Codex           |     ✅    |     ✅     |        ✅        |       ✅       |        ✅       |       ✅      |      ✅     |       ✅       |      ✅      |        ✅       |   ✅   |
| Antigravity CLI |     ✅    |     ✅     |        ✅        |       ✅       |        ✅       |       ✅      |      ✅     |       ✅       |      ✅      |        ✅       |   ✅   |
| Gemini          |     ✅    |     ✅     |        ✅        |       ✅       |        ✅       |       ✅      |      ✅     |       ✅       |      ✅      |        ✅       |   ✅   |
| Kiro            |     ✅    |     ✅     |        ✅        |       ✅       |        ✅       |       ✅      |      ✅     |       ✅       |      ✅      |        ✅       |   —    |
| Copilot CLI     |     ✅    |     ✅     |        ✅        |       ✅       |        ✅       |       ✅      |      ✅     |       ✅       |      ✅      |        ✅       |   ✅   |
| Cursor          |     ✅    |     ✅     |        ✅        |       ✅       |        ✅       |       ✅      |      ✅     |       ✅       |      ✅      |        ✅       |   —    |
| Cline           |     ✅    |     ✅     |        ✅        |       ✅       |        ✅       |       ✅      |      ✅     |       ✅       |      ✅      |        ✅       |   —    |

## Usage

```bash
recall sync          # incremental sync (safe to run anytime)
recall sync --force  # reprocess every session (after changing embedding model)
recall               # launch TUI
recall search Q      # one-shot CLI search
recall info          # index stats and worker status
```

## License

[MIT](LICENSE)

## Acknowledgements

Thanks to [tokscale](https://github.com/junhoyeo/tokscale) for the usage dashboard reference and token accounting behavior.
