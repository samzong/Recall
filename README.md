# Recall

Search all your AI coding sessions in one place. Recall indexes conversations from Claude Code, OpenCode, Codex, Gemini CLI, and Kiro CLI into a local SQLite database, then lets you search them with a fast TUI powered by hybrid FTS + semantic retrieval.

[![Recall TUI](recall.png)](https://asciinema.org/a/909453)

## Install

```bash
brew install samzong/tap/recall
```

Or build from source:

```bash
cargo install --path .
```

Requires Rust 1.85+ (edition 2024).

## Usage

```bash
recall index    # build full index (first run)
recall sync     # incremental update
recall          # launch TUI
recall info     # show index stats and worker status
```

### TUI

Type to search. Results update in real-time with hybrid ranking: FTS5 keyword matching at message level + semantic vector search at session level, fused via RRF.

- `Tab` — cycle filters (source → time → sort)
- `Enter` — open full conversation
- `Ctrl+R` — resume the selected session in its original CLI (see [Resume](#resume) below)
- `Ctrl+S` — settings (toggle sources, sync window)
- `q` / `Esc` — back / quit

In conversation view:

- `j` / `k` — scroll messages
- `g` / `G` — jump to first / last message
- `/` — search within the conversation; `n` / `N` — next / previous match
- `c` — copy current message to clipboard
- `e` — export session to file
- `Ctrl+R` — resume this session in its original CLI

### CLI Search

```bash
recall search "auth middleware"
recall search "refactor" --source cc --time 7d
```

## Supported Sources

| Source | Label | Data Location | Resume |
|--------|-------|---------------|--------|
| Claude Code | CC | `~/.claude/projects/`, `~/.claude/transcripts/` | yes |
| OpenCode | OC | `~/.local/share/opencode/opencode.db` | yes |
| Codex | CDX | `~/.codex/sessions/` | yes |
| Gemini CLI | GEM | `~/.gemini/tmp/*/chats/` | no |
| Kiro CLI | KIRO | `<data_dir>/kiro-cli/data.sqlite3` | no |

Missing tools are skipped automatically.

### Resume

`Ctrl+R` on a selected session re-launches its original CLI to continue the
conversation. Sources marked `no` above don't support this: their CLIs do not
expose a way to resume a specific conversation by ID. Gemini CLI's `--resume`
flag takes `latest` or an index from `--list-sessions` rather than the internal
session ID. Kiro CLI's `chat --resume` only rewinds to the most recent
conversation in the current directory, with no per-session selector. Recall
surfaces a status message instead of launching the wrong conversation.

## How It Works

1. **Index** — Adapters scan each tool's local storage, extract sessions and messages
2. **Store** — Sessions and messages go into SQLite with FTS5 full-text indexes
3. **Embed** — Session summaries are embedded locally via multilingual-e5-small (384-dim), stored in sqlite-vec
4. **Search** — Queries hit both FTS5 (keyword) and sqlite-vec (semantic), results fused with Reciprocal Rank Fusion

All data stays local. No API calls, no cloud, no telemetry.

## License

[MIT](LICENSE)
