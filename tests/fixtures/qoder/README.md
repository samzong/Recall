# Qoder CLI parser fixtures

`real-main.jsonl` and `real-fork.jsonl` were captured from authenticated Qoder
CLI 1.1.65 on macOS 27.0.1 arm64 on 2026-10-07, using Qwen3.8-Flash in an
isolated HOME and a temporary project containing only `sample.txt`.
Three short tasks read the file, ran `wc -l sample.txt`, appended `gamma`,
resumed the main session to ask which line was appended, and forked the session.
The file changed from `alpha` / `beta` to `alpha` / `beta` / `gamma`.

The main transcript contains six visible messages and five tool calls with five
results. Resume returned `gamma`. The fork copies those messages and tools,
adds two visible messages, and records `forkedFrom` pointing to the main session.
The fork request asked for `FORK_OK`; the actual assistant instead answered
`The final line count in sample.txt is 3.`. Both the request and actual answer
are preserved. Native-provider input, output, cache-read, and cache-write counts
were all zero; these real fixtures must not generate usage events.

Sanitization keeps probe messages, tool payloads/results, timestamps, inline
titles, ancestry, and zero token counters. Session IDs, record UUIDs, model
response IDs, and tool-call IDs are replaced consistently; project paths become
`/tmp/qoder-real-fixture-project`. Reasoning text is redacted. Runtime
configuration, file-history snapshots, account and billing metadata, request
identifiers, and other unrelated records are omitted. No credentials or original
login conversation are included. The parser tests exercise the sanitized real
envelopes, copied fork history, resume messages, tool result linkage, and hidden
usage behavior.

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

Positive token values, cache splitting, compaction/rewind behavior, subagent
names, and worktree/relocation priority remain synthetic or bundle-derived
regression coverage. The authenticated real captures do not establish those
numeric or runtime behaviors, China-edition behavior, or IDE storage formats.
