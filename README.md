# Recall

> Local-first search across every AI coding session on your machine.

[![Recall TUI](recall.png)](https://asciinema.org/a/909453)

You bounce between Claude Code, Codex, Copilot CLI, and whatever comes next. Each tool keeps its own sessions in its own place, in its own format. Recall pulls them all into one local index you can actually search — and drops you right back into any session in its original CLI.

## Install

```bash
brew install samzong/tap/recall
# or
make install # clone
```

## Support

One index across every AI coding CLI. Sync once, search everywhere, resume right where you left off.

| Capability             | Claude Code | OpenCode | Codex | Gemini | Kiro | Copilot CLI |
| ---------------------- | :---------: | :------: | :---: | :----: | :--: | :---------: |
| Auto-discovery         |     ✅      |    ✅    |  ✅   |   ✅   |  ✅  |     ✅      |
| Full index             |     ✅      |    ✅    |  ✅   |   ✅   |  ✅  |     ✅      |
| Incremental sync       |     ✅      |    ✅    |  ✅   |   ✅   |  ✅  |     ✅      |
| FTS5 keyword search    |     ✅      |    ✅    |  ✅   |   ✅   |  ✅  |     ✅      |
| Semantic search        |     ✅      |    ✅    |  ✅   |   ✅   |  ✅  |     ✅      |
| Source filter          |     ✅      |    ✅    |  ✅   |   ✅   |  ✅  |     ✅      |
| Time range filter      |     ✅      |    ✅    |  ✅   |   ✅   |  ✅  |     ✅      |
| In-session search      |     ✅      |    ✅    |  ✅   |   ✅   |  ✅  |     ✅      |
| Copy message           |     ✅      |    ✅    |  ✅   |   ✅   |  ✅  |     ✅      |
| Export to Markdown     |     ✅      |    ✅    |  ✅   |   ✅   |  ✅  |     ✅      |
| Resume in original CLI |     ✅      |    ✅    |  ✅   |   —    |  —   |     ✅      |

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
