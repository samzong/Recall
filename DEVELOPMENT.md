# Adding a New Source Adapter

Recall discovers AI coding sessions through **source adapters**. Each adapter knows how to find and parse one tool's session data. Adding a new one requires exactly two files touched.

## 1. Create the adapter

Create `src/adapters/<tool>.rs`:

```rust
use crate::adapters::{RawMessage, RawSession, SourceAdapter};
use crate::types::Role;

pub struct MyToolAdapter;

impl SourceAdapter for MyToolAdapter {
    fn id(&self) -> &str { "my-tool" }       // stored in DB, used for filtering
    fn label(&self) -> &str { "MT" }          // short label shown in TUI

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        // Return empty vec if the tool is not installed
        let data_dir = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("no home dir"))?
            .join(".my-tool");
        if !data_dir.exists() {
            return Ok(vec![]);
        }

        let mut sessions = Vec::new();

        // Parse session files / databases here...
        // For each session found:
        sessions.push(RawSession {
            source_id: "unique-session-id".to_string(),  // tool's native session ID
            directory: Some("/path/to/project".to_string()),
            started_at: 1700000000000,                    // Unix timestamp in milliseconds
            updated_at: None,
            entrypoint: None,
            messages: vec![
                RawMessage {
                    role: Role::User,
                    content: "user message text".to_string(),
                    timestamp: None,
                },
                RawMessage {
                    role: Role::Assistant,
                    content: "assistant response text".to_string(),
                    timestamp: None,
                },
            ],
        });

        Ok(sessions)
    }
}
```

## 2. Register it

In `src/adapters/mod.rs`, add two lines:

```rust
pub mod my_tool;  // add module declaration
```

```rust
pub fn all_adapters() -> Vec<Box<dyn SourceAdapter>> {
    vec![
        Box::new(claude_code::ClaudeCodeAdapter),
        Box::new(opencode::OpenCodeAdapter),
        Box::new(codex::CodexAdapter),
        Box::new(my_tool::MyToolAdapter),  // add to registry
    ]
}
```

That's it. The DB schema, search engine, TUI source filter, and CLI `--source` flag all pick it up automatically.

## Contract

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `id()` | `&str` | yes | Lowercase, kebab-case. Stored in SQLite `sessions.source` column. |
| `label()` | `&str` | yes | 2-4 uppercase chars. Shown in TUI session list and filter bar. |
| `source_id` | `String` | yes | The tool's native session identifier. Must be unique per source. |
| `started_at` | `i64` | yes | Unix timestamp in **milliseconds**. |
| `messages` | `Vec<RawMessage>` | yes | Ordered by time. Only `User` and `Assistant` roles. |

## Guidelines

- If the tool is not installed, return `Ok(vec![])` -- never error on missing data.
- Open external databases read-only (`SQLITE_OPEN_READ_ONLY`) to avoid locking the user's data.
- Extract only `text` content. Skip tool calls, images, and internal metadata.
- Use `tracing::warn!` for recoverable parse errors, skip the session, and continue.

## Verify

```bash
make check                  # must pass before push — same gate as CI
make index                  # should show "Scanning MT..." with session count
make search Q="test --source mt"
make run                    # TUI filter should include MT
```

## CI

CI runs `make check` — the same single command you run locally. There is no separate CI-only logic.

```
make check = cargo fmt --check → cargo clippy → cargo test
```

Always run `make check` before pushing. If it passes locally, CI will pass.
