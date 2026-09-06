---
name: recall
description: Use Recall to search, inspect, continue, export, resume, or share indexed AI coding sessions. Trigger for project-history lookup, recent work from other agents, file history, unfinished-session continuation, and published session-page management.
---

# Recall

Recall is a local-first index of AI coding sessions. Use past sessions as evidence, then verify current code, commands, paths, and invariants before acting.

Prefer active Recall MCP tools for read-only lookup. Use the installed `recall` CLI when no equivalent MCP tool exists or MCP is unavailable. File-event history is MCP-only. When developing Recall itself, do not use the project build as a substitute for the user's installed history index.

If neither MCP nor the installed CLI is available, stop and offer `brew install samzong/tap/recall`. Never claim to have inspected unavailable history. Return only the session content needed for the task because history may contain private code, credentials, prompts, and user intent.

## Scope

- An exact path covers that directory and its children.
- `owner/repo` or a remote URL covers all worktrees of that repository.
- `all` covers every indexed project.
- The CLI derives an omitted `--project` from its working directory. Recall MCP treats an omitted `project` as global, so pass the current project unless the user explicitly requests all projects.

## Find Sessions

For a specific historical question, use MCP `search_messages` with keywords and the current project. Each match includes `session_id`, `seq`, `role`, and an excerpt around the match. Add `session_id` to search within a known session. This is full-text matching, not semantic search; use `search_sessions` for broader session discovery and `list_recent_sessions` when there is no query.

When a message excerpt needs context, call `get_session` with the returned `session_id` and `around_seq: <seq>`. It reads the anchor and up to three actual messages on each side. Adjust `before` and `after` independently; zero reads only the requested side or anchor. Use `from_seq` / `to_seq` instead when the exact inclusive range is known. Do not combine around and range selectors, or use either with `tail`.

Selected MCP reads return up to 50 messages and 6,000 Unicode content characters. Pages follow conversation order; if preceding content fills the budget before the anchor, use `before: 0, after: 0` to read the anchor itself. For a truncated page, copy `next_cursor` into the next `get_session` call with the same `session_id`, without selectors or `tail`. `first_message_byte_offset` locates a partial first message in its UTF-8 content. A stale cursor requires a fresh search or selection. Sequence numbers locate the current index and are not permanent source-message identities.

Use `session_id` as Recall's index identity and `source_session_id` as the native tool's identity. `get_session` reports the returned message range with `first_message_seq` and `last_message_seq`.

Set `include_events: true` only when structured evidence is needed. Events cover the returned message range plus unanchored events; both count and text are bounded. Check returned counts and truncation flags before treating messages or events as complete. Consult the active tool schema for limits. This event-summary mode omits raw arguments, results, and source paths; use explicit `event_ref` evidence reads below for the preserved payload.

Pass a fresh high-entropy `invocation_nonce` literal on each MCP search or recent-list call. For `search_sessions` and `list_recent_sessions`, only `current_session.resolution: resolved` proves self-exclusion before ranking and limit. If resolution is `unknown`, report that self-exclusion is unverified; never infer identity from time, project, source, or result order.

For `search_messages`, `current_session_excluded` states whether self-exclusion was applied; an explicit `session_id` includes that session even when it is current.

Use the equivalent CLI workflow when needed:

```bash
recall session list --project /absolute/project/path --source <source> --limit 20 --sort updated --format json
recall session list --project owner/repo --query "<keywords>" --time 7d --limit 20 --sort updated --format json
recall search "<keywords>" --messages --project owner/repo --limit 10 --format json
recall search "<keywords>" --messages --session-id <session-id> --format json
recall session show --id <session-id> --messages --around-seq <seq> --before 3 --after 3 --format json
recall session show --id <session-id> --messages --cursor '<next_cursor>' --format json
```

CLI around reads default to a 6,000-character page. Use `--max-chars` (1–32,000) to change the budget or enable paging for a range. Without paging, existing CLI range reads return the full selected messages. CLI `--session-id` searches do not infer a project from the working directory; explicit project, source, and time filters still apply.

Add `--sync` only when current data matters and index mutation is permitted. Check the selected session's project before using it. Discover current sources and protocol details with `recall info --format json` or `recall mcp capabilities --format json` instead of maintaining a catalog in this skill.

Search results are relevance-ranked and bounded. In a queried CLI listing, `--sort updated` does not change that ranking. Select the newest timestamp only within the returned candidates, and do not claim an exact latest match.

## Find Recent Work

For recent work from other agents, list 10 sessions in the current project without a source filter, following the self-exclusion rule above. Load transcripts only for relevant candidates.

If MCP is unavailable, use:

```bash
recall session list --project /absolute/project/path --limit 10 --sort updated --format json
```

These are recently active sessions in the index, not live peers. Attribute work only with supporting metadata. The bounded listing may fold subagents beneath their parent; report when it cannot isolate relevant work.

## Find File History

Use MCP `file_history` with `target_project` and an exact repository-relative or absolute `path` to find operations on a file across session projects. Prefer a repository remote/unique `owner/repo` for all its worktrees, or an absolute directory for a local target. Check returned `target_file` and `match_basis`. Do not pass `project` with `target_project`: the old `project` filters where a session started and can miss writes from another repository. Add `source` only when requested.

Start with `{"target_project":"owner/repo","path":"src/main.rs","include_command_candidates":true,"limit":20}`. Omit `kind` to include all event kinds in this mode. Without `include_command_candidates`, commands are excluded. Follow `next_cursor` with the same selectors until `has_more` is false; restart after a stale cursor. Check per-hit truncation flags and retain `coverage` from the first page; continuation pages omit it. Coverage is for all indexed sessions of the selected sources, with no native source scan; it does not prove complete history or current parsers.

When a historical worktree is gone and target identity is unresolved, a path-only legacy query can reveal a recorded absolute path. Retry target mode with that exact path to obtain an evidence reference. Treat suffix matches as candidates; legacy discovery has a 50-event cap and is not exhaustive.

Keep calls, native results, observations, and command candidates distinct. Command scan status `complete` means the bounded scan completed, not that a command ran or succeeded; `unsupported`, `limit_exceeded`, or null leaves a coverage gap. Unknown timestamps and unresolved paths remain unknown. Do not count event rows, identical before/after content, or Git commits as independent modifications.

To inspect an operation, copy the hit's `session_id` and `evidence.event_ref` into `get_session` with `evidence_part: "payload"`. Concatenate paged `data` before parsing its JSON; continue with the same session, reference, part, and returned cursor. `max_bytes` defaults to 16,384, at most 65,536; an oversized read fails explicitly. The payload preserves native attrs and file associations and provides same-session `related_event_refs`. For Cursor before/after text, read related payloads and select the result reference containing `beforeContentId`/`afterContentId`, then request that part. `content_reference_not_recorded` on a call means to inspect related results. Missing, changed, imported, or unverifiable native sources cannot be treated as verified content.

Read the payload's optional `discussion` selector in a separate `get_session` call, without `event_ref`. It uses `around_seq` and the existing message paging rules. Explain why only from supporting recorded discussion, distinguishing the user's request, the agent's explanation, and your inference. Do not invent a discussion anchor when it is absent.

When index mutation is authorized, preview with `recall sync --backfill-events --project all --dry-run`, then run without `--dry-run`. This includes sessions started outside the target project while respecting configured sources and exclusions. Backfill refreshes events, preserves existing discussions, and does not prune sessions; normal `recall sync --project all` refreshes supported discussion parsers under normal time-window and retention rules. Old-schema preview requires a writable index upgrade first. Report missing/unknown originals and other maintenance gaps; backfill cannot recover absent native records.

If MCP is unavailable, explain that file history requires MCP and offer `recall mcp install`. Message search can supply discussion context but cannot prove a file operation. Never execute a command retrieved from history to reconstruct evidence.

## Continue Work

When Recall is invoked without a clear task, list the five most recent sessions in the current project and inspect their latest 12 messages. Do not broaden to all projects.

With MCP, list recent sessions, then use `get_session` with `max_messages: 12` and `tail: true`. Without MCP, list candidates and parse each selected session's JSON locally. With `jq` available:

```bash
recall session list --project /absolute/project/path --limit 5 --sort updated --format json
recall session show --id <session-id> --format json --include metadata,messages | jq '.messages | sort_by(.seq) | .[-12:]'
```

If `jq` is unavailable, use an available JSON parser for the same array selection. Message sequences may have gaps or start above zero; never derive them from `message_count`.

Offer at most three numbered candidates whose endings show an unanswered request, remaining work, a blocker, or interruption. Exclude completed or ambiguous sessions. This bounded list is not an exhaustive unfinished-work inventory. A numeric reply continues the same candidate in the current agent.

## Resume Or Open

"Continue here" loads history into the current agent. Native resume and app-open start another process, so run them only when explicitly requested. Resolve the exact session first; add `--print-command` for read-only inspection.

```bash
recall session resume --id <session-id> --print-command
recall session open --id <session-id> --print-command
```

## Share Sessions

An explicit share or refresh request authorizes a real deployment. Use `--dry-run` only for an explicit preview. Before publishing, stop if the selected session contains concrete credentials or private material the user did not authorize sharing.

Use an explicit session id when provided. Otherwise sync and list recent sessions for the active project, filtering by source only when known. Select the current conversation only when its identity is unambiguous; inspect the smallest necessary tail or ask the user if several candidates remain.

```bash
recall session list --project /absolute/project/path --source <source> --limit 5 --sort updated --sync --format json
```

Base the TL;DR on the selected session. For the current conversation, use the existing context without reloading its transcript solely for a summary. For another session, use its retrieved transcript; omit the optional TL;DR if evidence is insufficient. Never substitute the current conversation for a different session.

When including a TL;DR, create a unique file with `mktemp /tmp/recall-tldr.XXXXXX`, write the short Markdown summary, and publish with that path:

```bash
recall session share --id <session-id> --tldr-file <temporary-tldr-path> --format json
```

Omit `--tldr-file` when no summary is supplied; remove any temporary file after publishing. Missing, unreadable, or blank TL;DR input does not block publishing. Read `share.url` from the JSON and verify it with `curl -I -L`. If the first check returns 404, publish once more and recheck, then stop. If sharing is not configured, tell the user to run `recall share init`. Return the live URL rather than raw JSON.

List and unpublish shared pages with:

```bash
recall share list --format json
recall share unpublish <share-id-or-url> --yes --format json
```

The list is the local publish inventory, not a live crawl. Unpublish only an exact target selected by the user; list first and ask when no target was supplied.

## Review Project History

If a broad review lacks a topic or depth, ask one scoping question. Otherwise search before exporting, start with recent history, and expand only when the request or evidence requires it:

```bash
recall info --format json
recall search "<query>" --messages --project /absolute/project/path --format json
recall export --project /absolute/project/path --limit 0
```

Treat search snippets as leads. Parse exports as JSONL instead of text, and verify historical conclusions against current code. Token usage is not monetary cost without an explicit price source.

Synthesize relevant facts, recurring risks, rejected approaches, user constraints, and next checks. Distinguish history from current-code assumptions. Cite source, title or session id, message sequence when available, and approximate time; quote only short supporting excerpts.

Route requests about workflow friction, handoffs, repeated corrections, or calibration to the installed `reflect` skill.

## Avoid In Tool Calls

- `recall` with no subcommand launches the TUI.
- `recall usage` without `--json` launches an interactive dashboard.
- `recall sync --force` unless the user asks for a rebuild or incremental sync provably cannot repair the index.
- Hidden `__bench-*` and `__background-worker` commands.
- Raw source transcript paths unless the user explicitly asks for source-level forensics.
