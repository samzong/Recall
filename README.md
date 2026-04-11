# Recall

> Local-first search across every AI coding session on your machine.

[![Recall TUI](recall.png)](https://asciinema.org/a/909453)

## Install

```bash
brew install samzong/tap/recall
# or
make install # clone
```

## Support

| Capability             | Claude Code | OpenCode | Codex | Gemini | Kiro | Copilot CLI |
| ---------------------- | :---------: | :------: | :---: | :----: | :--: | :---------: |
| Auto-discovery         |     ✅      |    ✅    |  ✅   |   ✅   |  ✅  |     ✅      |
| Full index             |     ✅      |    ✅    |  ✅   |   ✅   |  ✅  |     ✅      |
| Incremental sync       |     ✅      |    ✅    |  ✅   |   ✅   |  ✅  |     ✅      |
| FTS5 keyword search    |     ✅      |    ✅    |  ✅   |   ✅   |  ✅  |     ✅      |
| Semantic search        |     ✅      |    ✅    |  ✅   |   ✅   |  ✅  |     ✅      |
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
