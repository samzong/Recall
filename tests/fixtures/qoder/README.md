# Qoder CLI parser fixture

`cli.jsonl` is synthetic, not captured from an authenticated Qoder session.
It contains no credentials or private conversation data.

The envelope, message blocks, inline titles, and active-leaf records follow the
published `@qoder-ai/qodercli` 1.1.65 npm bundle (`bundle/qodercli.js`):
`keo` writes cwd, sessionId, parentUuid, isSidechain and RFC3339 timestamps;
`jW` / `Xo` create assistant / user records; `KTl` persists assistant usage for
custom models and strips requestId; `_sl` puts usage on the final response block;
`av` hides native-provider token counts. Numbers and message contents here are
invented for assertions, not measured token consumption.

Regression inputs in the adapter tests follow `Ogi` / `nVA` / `OM` / `UQc` for
cache-inclusive usage, `keo` for `logicalParentUuid` at compaction boundaries,
and `buildForkEntries` for per-record `forkedFrom` ancestry. Original functions
extracted from the bundle confirm that Anthropic, Chat, and Responses input
counts include cache read and cache write before persistence.
`Ob` permits plugin-qualified and user-defined agent names in subagent filenames.
`cco` selects worktree cwd (restoring original cwd after exit), relocation cwd,
then the latest message cwd; directory-priority tests follow these records.
No `-session.json` or `fork_from` writer exists in this bundle; only transcript
metadata is supported.

Installation on macOS succeeded, but print mode, model discovery, and the
interactive startup all required vendor authentication. Qoder itself recognized this synthetic transcript through `--list-sessions`.
No vendor account was registered or used. Replace or supplement this fixture with a sanitized real
session once authorized credentials are available.
