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

Use MCP `list_recent_sessions` without a query, `search_sessions` with a query, and `get_session` only when transcript evidence is needed.

Use `session_id` as Recall's index identity and `source_session_id` as the native tool's identity. `get_session` reports the returned message range with `first_message_seq` and `last_message_seq`.

Set `include_events: true` only when structured evidence is needed. Events cover the returned message range plus unanchored events; both count and text are bounded. Check returned counts and truncation flags before treating messages or events as complete. Consult the active tool schema for limits; raw arguments, results, and source paths are not returned.

Pass a fresh high-entropy `invocation_nonce` literal on each MCP search or recent-list call. Only `current_session.resolution: resolved` proves self-exclusion before ranking and limit. If resolution is `unknown`, report that self-exclusion is unverified; never infer identity from time, project, source, or result order.

Use the equivalent CLI workflow when needed:

```bash
recall session list --project /absolute/project/path --source <source> --limit 20 --sort updated --format json
recall session list --project owner/repo --query "<keywords>" --time 7d --limit 20 --sort updated --format json
recall session show --id <session-id> --format json --include metadata,messages
```

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

Use MCP `file_history` for requests about sessions that touched a path. Pass `path` and the current `project`; add `source` only when requested. Omit `kind` to include the default `file_write` and `file_read` events, and default `limit` to 20.

Run `recall sync --project <same-scope>` first only when recent writes may be absent and index mutation is permitted. If MCP is unavailable, explain that file history requires MCP and offer `recall mcp install`. Transcript search and raw transcript inspection are not substitutes for file-event targets.

Return the matching event rows with session, source, title, kind, target, and time. The limit applies to recent events, not distinct sessions, so do not claim exhaustive session coverage. Load transcript evidence only when needed.

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
recall search "<query>" --project /absolute/project/path --format json
recall export --project /absolute/project/path --limit 0
```

Treat search snippets as leads. Parse exports as JSONL instead of text, and verify historical conclusions against current code. Token usage is not monetary cost without an explicit price source.

Synthesize relevant facts, recurring risks, rejected approaches, user constraints, and next checks. Distinguish history from current-code assumptions. Cite source, title or session id, and approximate time; quote only short supporting excerpts.

Route requests about workflow friction, handoffs, repeated corrections, or calibration to the installed `reflect` skill.

## Avoid In Tool Calls

- `recall` with no subcommand launches the TUI.
- `recall usage` without `--json` launches an interactive dashboard.
- `recall sync --force` unless the user asks for a rebuild or incremental sync provably cannot repair the index.
- Hidden `__bench-*` and `__background-worker` commands.
- Raw source transcript paths unless the user explicitly asks for source-level forensics.
