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

MCP search and recent hits expose `session_id` as Recall's index identity and `source_session_id` as the source tool's session identity. `get_session` returns both identities plus `first_message_seq` and `last_message_seq` for the messages represented in its text; both sequence fields are null when no messages are returned.

Use the equivalent CLI workflow when needed:

```bash
recall session list --project /absolute/project/path --source <source> --limit 20 --sort updated --format json
recall session list --project owner/repo --query "<keywords>" --time 7d --limit 20 --sort updated --format json
recall session show --id <session-id> --format json --include metadata,messages
```

Add `--sync` only when current data matters and index mutation is permitted. Check the selected session's project before using it. Discover current sources and protocol details with `recall info --format json` or `recall mcp capabilities --format json` instead of maintaining a catalog in this skill.

Search results are relevance-ranked and bounded. In a queried CLI listing, `--sort updated` does not change that ranking. Select the newest timestamp only within the returned candidates, and do not claim an exact latest match.

## Find Recent Work

When the user asks what other agents recently did, call `list_recent_sessions` for the current project with no source filter and a limit of 10. Exclude the current conversation when identifiable and call `get_session` only for relevant candidates.

If MCP is unavailable, use:

```bash
recall session list --project /absolute/project/path --limit 10 --sort updated --format json
```

The result proves only that sessions were recently indexed. Do not describe them as live peers or attribute them to another agent without supporting metadata. The listing may fold spawned subagents beneath a visible parent, so it is not an exhaustive peer inventory. Report when the bounded result cannot isolate relevant work.

## Find File History

Use MCP `file_history` for requests about sessions that touched a path. Pass `path` and the current `project`; add `source` only when requested. Omit `kind` to include the default `file_write` and `file_read` events, and default `limit` to 20.

Run `recall sync --project <same-scope>` first only when recent writes may be absent and index mutation is permitted. If MCP is unavailable, explain that file history requires MCP and offer `recall mcp install`. Transcript search and raw transcript inspection are not substitutes for file-event targets.

Return the matching event rows with session, source, title, kind, target, and time. The limit applies to recent events, not distinct sessions, so do not claim exhaustive session coverage. Load transcript evidence only when needed.

## Continue Work

When Recall is invoked without a clear task, list the five most recent sessions in the current project and inspect their latest 12 messages. Do not broaden to all projects.

With MCP, call `list_recent_sessions`, then `get_session` with `max_messages: 12` and `tail: true`. Without MCP, use:

```bash
recall session list --project /absolute/project/path --limit 5 --sort updated --format json
recall session show --id <session-id> --format json --include metadata,messages --from-seq <calculated-sequence>
```

Set `<calculated-sequence>` to `max(message_count - 12, 0)` from the list result. Offer at most three numbered candidates. Include only sessions whose ending contains an unanswered request, explicit remaining work, a blocker, or interrupted execution. Exclude completed and ambiguous sessions. Exclude the current session when identifiable; otherwise state that self-exclusion could not be verified. Treat the bounded list as candidates, not a complete inventory of unfinished work. A numeric reply selects the same candidate and continues it in the current agent.

Do not launch native resume or app-open behavior unless the user explicitly requests it.

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

Create a unique temporary file with `mktemp /tmp/recall-tldr.XXXXXX`, write a short Markdown TL;DR from the current conversation context, and do not reload the transcript just to summarize it. Then publish with that path:

```bash
recall session share --id <session-id> --tldr-file <temporary-tldr-path> --format json
```

Remove the temporary file after publishing. Missing, unreadable, or blank TL;DR input does not block publishing. Read `share.url` from the JSON and verify it with `curl -I -L`. If the first check returns 404, publish once more and recheck, then stop. If sharing is not configured, tell the user to run `recall share init`. Return the live URL rather than raw JSON.

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

Return project memory, not a list of matching sessions. Cite source, title or session id, and approximate time for each fact. Summarize transcripts and quote only short excerpts that serve as evidence. For a broad review, use this shape:

```text
Recall review of <project>:

1. Historical facts that matter now
2. Repeated risks or unresolved problems
3. Failed or rejected approaches to avoid
4. User or project constraints extracted from history
5. Current code assumptions to verify
6. Recommended next checks
```

Route requests about workflow friction, handoffs, repeated corrections, or calibration to the installed `reflect` skill.

## Avoid In Tool Calls

- `recall` with no subcommand launches the TUI.
- `recall usage` without `--json` launches an interactive dashboard.
- `recall session share --dry-run` when the user asked to share or refresh a link, and returning a URL without a real publish.
- `recall share unpublish` without an exact target selected by the user.
- `recall sync --force` unless the user asks for a rebuild or incremental sync provably cannot repair the index.
- Hidden `__bench-*` and `__background-worker` commands.
- Raw source transcript paths unless the user explicitly asks for source-level forensics.
