# Warp fixtures

`probe.textproto` and `probe.bin` are sanitized derivatives of a real local
conversation produced on 2026-10-07 with Warp 0.2026.09.30.08.29.01 and Grok 4.7
(`grok-4-7-low`). The user requested reading `sample.txt`, running `wc -l`, and
appending `gamma` in `/tmp/recall-warp-probe`. The terminal output changed from
two lines to three, and the final file contained `alpha`, `beta`, and `gamma`.

The user exported a consistent SQLite snapshot using `.backup` from Warp's own
terminal. Only the probe task was decoded using the official protocol revision
linked below. Message, task, request, and call IDs were replaced with fixture
aliases. Only allowlisted prompt/reply text, title, temporary-project paths,
commands, shell output/exit codes, timestamps, model markers, tool variants,
and query-origin markers were retained. Account/user identifiers, server data,
conversation tokens, credentials, project rules, reasoning text, attachments,
and result contexts were omitted. Read-success and ignored server/reasoning
payloads retain their original variants with empty bodies. The sanitized task
was serialized with libprotoc 36.1; it is not an unmodified original blob.

This task contains one user message, one assistant message, four calls (including
a server call), and three results. It has no `RequestMetadata`, so Recall emits
no usage events. The source does not record a parent conversation. Both shell
commands and their successful results are preserved. The shared command-evidence
parser does not support `wc` or ordinary output redirection, so this conversation
has read-call file evidence but no inferred shell file evidence.

The snapshot contained three conversations and two tasks. The other conversations
were checked only for indexing status and are absent from these fixtures. The
snapshot and temporary private indexes were deleted after verification.

`task.*`, `origins.*`, and `schema.sql` remain synthetic fixtures.

`schema.sql` is a minimal synthetic schema based on the upstream migration that
introduced `agent_conversations` and `agent_tasks`. It retains the column types
read by the adapter, without copying the application's triggers or indexes.
Source: [Warp persistence migration](https://github.com/warpdotdev/warp/blob/d807d987652361ea970287748fe4f2ea002766cc/crates/persistence/migrations/2025-06-09-013710_create_agent_conversations_table/up.sql).

`task.textproto` describes a user prompt, two assistant replies, file read, shell
command, file edit, tool results, ignored system/debug messages, and explicit
per-request token counts. `task.bin` was encoded with `libprotoc 36.1` using
[Warp's official task protocol](https://github.com/warpdotdev/warp-proto-apis/blob/00cdd6726f28fb6b32a6712f0ca3264dbd0956a1/apis/multi_agent/v1/task.proto).
It is independent of Recall's partial protobuf structs. Unknown protobuf fields
are deliberately present to exercise forward-compatible decoding.

`origins.textproto` and `origins.bin` use the same official encoder and protocol
revision. They cover all eight `UserQueryOrigin` variants, absent attribution,
and an empty origin. Parent-agent, agent-message wake, schedule, automation,
and server-synthesized queries are excluded from user messages. Public API and
unknown origins remain readable because the origin records a delivery mechanism,
not proof of a non-human author:
[origin attribution](https://github.com/warpdotdev/warp-proto-apis/blob/00cdd6726f28fb6b32a6712f0ca3264dbd0956a1/apis/multi_agent/v1/attribution.proto).

Warp persists each task with `encode_to_vec()`:
[persistence writer](https://github.com/warpdotdev/warp/blob/d807d987652361ea970287748fe4f2ea002766cc/app/src/persistence/agent.rs).
The input/output/cache token fields are separate counts; Warp adds all four to
compute total tokens:
[usage accounting](https://github.com/warpdotdev/warp/blob/d807d987652361ea970287748fe4f2ea002766cc/crates/persistence/src/model.rs).

The application-created snapshot has the same adapter-read column types as
`schema.sql`, with an additional `summary` column in `agent_conversations`.
Real CLI verification used the macOS Application Support Stable fallback path
inside a sandbox HOME.
