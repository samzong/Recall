# Warp fixtures

These are synthetic fixtures, not transcripts from an authenticated Warp session.

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

The installed Warp 0.2026.09.30.08.29.01 could not finish launching on the test
machine. Its embedded `CREATE TABLE agent_conversations` and `CREATE TABLE
agent_tasks` migration statements were inspected and match these read column
types. No local database was created by the app, and no locally produced agent
session was available. This fixture is not an installed database dump or a real
conversation.
