# Adding a New Source Adapter

Recall discovers AI coding sessions through **source adapters**. A source adapter is
the single entry point for a tool. Search is the base capability; usage is an
optional extension on the same adapter.

Start with search. Add usage only when the tool exposes token data.

## 1. Add Search Support

Create `src/adapters/<tool>.rs`:

```rust
use crate::adapters::{RawMessage, RawSession, SourceAdapter};
use crate::types::Role;

pub(crate) struct MyToolAdapter;

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

        // Parse session files or databases here...
        // For each session found:
        sessions.push(RawSession::search_only(
            "unique-session-id",                         // tool's native session ID
            Some("/path/to/project".to_string()),
            1700000000000,                               // Unix timestamp in milliseconds
            None,
            None,
            vec![
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
        ));

        Ok(sessions)
    }
}
```

## 2. Register it

In `src/adapters/mod.rs`, add two lines:

```rust
pub(crate) mod my_tool;  // add module declaration
```

```rust
pub(crate) fn all_adapters() -> Vec<Box<dyn SourceAdapter>> {
    vec![
        Box::new(claude_code::ClaudeCodeAdapter),
        Box::new(opencode::OpenCodeAdapter),
        Box::new(codex::CodexAdapter),
        Box::new(my_tool::MyToolAdapter),  // add to registry
    ]
}
```

That's it. The DB schema, search engine, TUI source filter, and CLI `--source` flag all pick it up automatically.

## Search Contract

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `id()` | `&str` | yes | Lowercase, kebab-case. Stored in SQLite `sessions.source` column. |
| `label()` | `&str` | yes | 2-4 uppercase chars. Shown in TUI session list and filter bar. |
| `source_id` | `String` | yes | The tool's native session identifier. Must be unique per source. |
| `started_at` | `i64` | yes | Unix timestamp in **milliseconds**. |
| `messages` | `Vec<RawMessage>` | yes | Ordered by time. Only `User` and `Assistant` roles. |

## Optional: Add Usage Support

Usage is not a separate adapter. It is token data attached to the same
`RawSession`.

Add a parser version:

```rust
const USAGE_PARSER_VERSION: u32 = 1;

impl SourceAdapter for MyToolAdapter {
    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    // ...
}
```

Then attach usage events to the session:

```rust
let usage_events = vec![/* RawUsageEvent values parsed from the same tool data */];

sessions.push(
    RawSession::search_only(source_id, directory, started_at, updated_at, entrypoint, messages)
        .with_usage(usage_events, USAGE_PARSER_VERSION),
);
```

If the adapter uses file mtimes to skip unchanged sessions, pass the same parser
version into file scanning:

```rust
file_scan::run_file_scan_with_options(
    store,
    "my-tool",
    since_ts,
    file_scan::FileScanOptions {
        usage_parser_version: Some(USAGE_PARSER_VERSION),
    },
    entries,
    parse_my_tool_session_for_entry,
)
```

This lets Recall backfill usage when the usage parser changes, even when the
session messages did not change.

Usage event rules:

| Field | Notes |
|-------|-------|
| `event_key` | Stable and unique within the session. |
| `model` / `provider` | Use source data. Use `unknown` only when the source does not record it. |
| token fields | Split into input, output, cache read, cache write, and reasoning tokens. |
| `token_source` | `Observed` for source-reported values, `Derived` for deltas from counters, `Estimated` only for estimates. |
| `parser_version` | Set to `USAGE_PARSER_VERSION`; bump it when usage parsing changes. |

## Guidelines

- If the tool is not installed, return `Ok(vec![])` -- never error on missing data.
- Open external databases read-only (`SQLITE_OPEN_READ_ONLY`) to avoid locking the user's data.
- Extract only `text` content. Skip tool calls, images, and internal metadata.
- Use `tracing::warn!` for recoverable parse errors, skip the session, and continue.
- Do not add usage support until the source exposes real token data.
- Do not create a separate usage adapter for the same tool.

## Verify

```bash
make check                  # must pass before push — same gate as CI
cargo run -- sync -v        # should show "Scanning MT..." with session count
make search Q="test --source mt"
cargo run -- usage --source my-tool   # only when usage support was added
make run                    # TUI filter should include MT
```

## CI

CI runs `make check` — the same single command you run locally. There is no separate CI-only logic.

```
make check = cargo fmt --check → cargo clippy → cargo test
```

Regression and eval harness tests live in `src/integration/` inside the library crate. Run them with:

```bash
cargo test integration::regression
cargo test integration::eval_harness
```

The former standalone targets `cargo test --test regression` and `cargo test --test eval_harness` are no longer used.

Always run `make check` before pushing. If it passes locally, CI will pass.

## Releases

Releases are driven by `cargo-release`, which bumps `Cargo.toml`, updates
`Cargo.lock`, commits, tags, and pushes in one step. The GitHub Actions
release workflow triggers on `v*` tag push and builds cross-platform
binaries.

Recall is not published to crates.io. `publish = false` in `Cargo.toml` is the
current application release boundary, not a package metadata bug.

### One-time setup

```bash
cargo install cargo-release
git config core.hooksPath .githooks   # enables auto DCO signoff
```

The `.githooks/prepare-commit-msg` hook appends `Signed-off-by` to every
commit, so both hand-written and `cargo-release`-driven commits satisfy the
project's DCO convention.

### Cut a release

```bash
make release-patch              # dry-run: shows what would happen
make release-patch EXECUTE=1    # apply: bump, commit, tag, push
```

`release-minor` and `release-major` work the same way. The tag name is
`v{{version}}` and the commit subject is `chore(release): bump to v{{version}}`.

### First release after a stale baseline

If `Cargo.toml` is at `0.1.0` but tags `v0.1.1`..`v0.1.3` already exist (because
earlier releases were tagged without bumping `Cargo.toml`), a patch bump will
collide with `v0.1.1`. Skip to the next free version explicitly:

```bash
cargo release 0.1.4              # dry-run
cargo release 0.1.4 --execute    # apply
```
