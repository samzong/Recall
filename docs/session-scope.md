# Session Scope

## Session Topology

Session topology records provenance without title/directory/time heuristics. It
appears in `session list`/`show` JSON and JSONL export under `session.topology`,
and both `recall session list` and bulk `recall export` accept
`--thread-role <primary|subagent|unknown>`.

Model (source-neutral):

- `thread_role`: `primary` (top-level user-owned execution), `subagent`, or
  `null` when the source gives no reliable classification. `primary` does not
  mean the session has no fork history.
- `parents[]`: portable `{ relation, source, source_id }` links. `relation` is
  `spawn`, `fork`, or `resume`. A session may have several parents. Missing
  parent sessions are valid — the unresolved portable identity is still exported.
  A `fork` relation alone is never proof of a subagent.

Per-adapter coverage:

| Source | Role | Parent links | Source signal |
| --- | --- | --- | --- |
| Codex | primary / subagent | spawn + fork | `session_meta.thread_source` / `source`, `parent_thread_id` / `thread_spawn`, `forked_from_id` |
| Claude Code | primary / subagent | spawn | `subagents/<agent>.jsonl` path + transcript `sessionId` parent |
| Pi | primary | fork | session-header `parentSession` |
| rx-agent | primary | none | version 1 session header; rx-agent records no parent relationship |
| Others (OpenCode, Cursor, Copilot, Gemini, Grok, Antigravity, Cline, Kiro) | `null` | none | not yet classified |

Grok retains its current skip-and-prune behavior; enabling classified Grok
ingestion is a separate search/TUI visibility review before the path is turned
on.

## Project Scope

Commands that operate on a set of sessions — `recall search`, `recall session
list`, `recall export` — resolve their scope in this order:

1. explicit `--project <selector>`;
2. otherwise the current directory: a Git checkout with a resolvable `origin`
   means that repository identity (so sibling worktrees are included), a Git
   checkout without one means the top-level directory, and anything else means
   every project.

`recall sync` and `recall session list --sync` use the same rule, and a
`--sync` listing always syncs exactly the scope it then shows.

An inferred non-global scope is reported on stderr, and every structured
response carries `filters.effective_scope` with `kind`
(`repository|directory|global`) and `value`. Pass `--project all` for the
explicit global scope; `--repo` is deprecated in favour of `--project`.

A scoped sync never writes, refreshes, or deletes a session outside its scope.
Two maintenance actions are therefore global-only and run on
`recall sync --project all` (or the background worker started by the TUI):
pruning sessions whose source transcript disappeared, and backfilling repo
identity for sessions indexed before it was resolvable. Deleting sessions that
match `excluded_paths` still runs, restricted to the current scope.

### Scheduled Sync (macOS)

```bash
recall sync schedule install --interval 30m
recall sync schedule status
recall sync schedule uninstall
```

The interval defaults to 30m; use an integer with `s`, `m`, or `h` units and a
minimum of 60 seconds. The LaunchAgent label is `io.github.samzong.recall.sync`;
its plist is `~/Library/LaunchAgents/io.github.samzong.recall.sync.plist` and
logs are `~/Library/Logs/recall/sync.stdout.log` and
`~/Library/Logs/recall/sync.stderr.log`.

The plist records the installing user's HOME so the job uses that user's
configuration and index.

Install runs the first sync immediately. macOS may prompt for access to multiple
folders in sequence; click Allow within 60 seconds for each prompt. If the first
sync fails, rerun `recall sync schedule install` (include `--interval` again to
retain a custom interval).

Git metadata probes time out after 60 seconds and abort the whole sync. The same
timeout applies to manual syncs and read commands that infer a project from the
current directory; read commands can bypass inference with `--project all`.
Linux and Windows report `not supported yet on <os>`.

Remote sync has a separate opt-in schedule:

```bash
recall remote sync schedule install --interval 30m
recall remote sync schedule status
recall remote sync schedule uninstall
```

It runs `recall remote sync` against the saved upload scope. Its label is
`io.github.samzong.recall.remote-sync`, with `remote-sync.stdout.log` and
`remote-sync.stderr.log` in the same log directory. It shares the local schedule's
interval parser and launchd management code, with independent intervals and
load state. Installing or uninstalling either schedule leaves the other intact.
Both jobs use the existing sync lock and wait when another sync holds it.
See [scheduled remote sync](remote-sync.md#scheduled-remote-sync-macos) for
background credential discovery, failure diagnostics, and launchd limitations.
