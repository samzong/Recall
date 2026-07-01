---
name: recall
description: Use Recall as a project memory layer for local AI coding session history. Trigger when the user mentions recall, wants a live share link (分享会话/分享对话/分享这个对话/给我链接/share session/session link), wants to refresh or update an existing share link (更新分享链接/刷新分享/重新分享/update share link), resume or open a session, search or export sessions, review project history, recover decisions, find failed approaches, or inspect Claude Code/Codex/OpenCode/Cursor/Grok session evidence.
---

# Recall

## Overview

Recall is a local-first CLI that indexes AI coding sessions from multiple tools. Treat it as a project memory layer: use it to recover history, reduce repeated mistakes, and turn prior sessions into current review evidence.

Use the installed `recall` command when inspecting the user's session history. If you are developing Recall itself, use the project build only for testing Recall behavior, not as a substitute for the user's installed history index.

If `recall` is unavailable, stop the Recall workflow, tell the user it is not installed, and offer `brew install samzong/tap/recall` instead of pretending history was inspected.

Recall history is evidence, not truth. Verify technical claims, code behavior, commands, paths, and invariants against the current repository before acting.

## Intent Routing

Pick the workflow from user intent. Do not run the full project-review scoping flow for one-shot session actions.

| Intent | Example prompts | Workflow |
| --- | --- | --- |
| Share a live link | "分享会话", "分享这个对话", "share this session", "给我链接" | Publish Or Refresh Share Link |
| Refresh an existing link | "用 recall 更新分享链接", "刷新分享", "重新分享", "update the share link" | Publish Or Refresh Share Link |
| Resume or open | "继续这个会话", "resume this chat" | Session Resume/Open |
| Find one session | "找上次讨论 migration 的会话", "当前项目最新 grok 会话" | Latest/Find Session Lookup |
| Review project history | "用 recall 审查这个项目", "历史风险" | Scoping Protocol + Analysis Modes |

Treat "share" and "update/refresh share link" as the same action: sync the latest transcript, publish to Pages, return the live URL.

## Project Scope Defaults

Treat "current project", "this repo", and similar wording as a repository identity request, not necessarily the current filesystem path. `recall --project` filters by exact session directory plus child paths only; it does not understand repo names, remotes, symlinks, or gmc worktrees.

Default scoping rules:

- If the user explicitly says global, all projects, or no project filter, do not scope by project.
- If the user gives an exact path, use that path with `--project`.
- If the request is about the current checkout only, use `git rev-parse --show-toplevel` with `--project`.
- If the request is about the current project/repo and may include other worktrees, derive repo identity first:
  - `git rev-parse --show-toplevel`
  - `git remote get-url origin`
  - repo slug such as `owner/repo` and repo name such as `repo`
- For repo-identity lookups, do not rely on `--project <current path>` alone. List a bounded recent candidate set with source/time filters, parse JSON structurally, and keep sessions whose project directory belongs to the same repo identity. Prefer matching candidates by running `git -C <session.project> remote get-url origin` when that directory still exists; fall back to exact repo-name basename matches only when the directory is unavailable.
- When reporting results from repo-identity filtering, say which project directories were included so path/worktree ambiguity is visible.

## Latest/Find Session Lookup

Use this workflow for one-shot requests such as "latest Grok session for this project" or "find the last session about X". Do not run the broad project-review scoping flow.

1. Determine source, recency, and project scope from the user wording.
2. If a single exact directory scope is intended, use:
   ```bash
   recall session list --project /absolute/project/path --source <source> --limit 20 --sort updated --sync --format json
   ```
3. If repo identity scope is intended, especially in gmc or alternate worktrees, use a bounded unscoped candidate list and filter structurally by repo identity:
   ```bash
   recall session list --source <source> --limit 100 --sort updated --sync --format json
   ```
   Increase the limit or add `--time 30d` only when needed. Do not pick the first global result without checking the session's `project`.
4. For a named or older session, add `--query "<keywords>"` and/or `--time 7d`, but keep the same project-scope rules.
5. Show the selected session with:
   ```bash
   recall session show --id <recall-session-id> --format json --include metadata,messages,usage,events
   ```

## Publish Or Refresh Share Link

When the user wants to share a session or update an existing share link, execute immediately. Do not ask scoping questions. Do not use `--dry-run` unless they explicitly ask to preview or validate only.

### What the user expects

- "分享会话" -> a **live, openable URL** for the current conversation.
- "用 recall 更新分享链接" -> **re-publish** the current conversation so the page reflects the latest messages. The URL usually stays the same; the deployed HTML changes.

### Steps

1. Infer the active source from the runtime when possible:
   - Grok -> `grok`
   - Cursor -> `cursor`
   - Codex -> `codex`
   - Claude Code -> `claude-code`
   - OpenCode -> `opencode`
   If the source is unclear, omit `--source` and rely on project + recency.

2. Sync and resolve the session id. Always include `--sync` so share/update uses the latest messages.
   - Current conversation (default):
     ```bash
     recall session list --project /absolute/project/path --source <source> --limit 1 --sort updated --sync --format json
     ```
   - Named or older session: add `--query "<keywords>"` and/or `--time 7d`, still prefer `--sort updated`.
   - Explicit id from the user: skip list and use that id directly.

3. Publish for real. This is mandatory for share/update requests:
   ```bash
   recall session share --id <recall-session-id> --format json
   ```
   Never stop at `--dry-run` for these requests. Dry-run only computes a future URL locally and leaves the live page at 404 or stale.
   Progress text goes to stderr; read the URL from stdout JSON at `share.url`.
   Add `--copy-url` when they ask to copy it; add `--open` when they ask to open it.

   Before publishing, write a short TL;DR markdown file from the current agent
   context and pass it with `--tldr-file`. Weight the user's request highest,
   weight the final outcome next, and use trace/process details only as
   low-weight context:
   ```bash
   # Agent writes /tmp/recall-tldr.md from the current conversation context.
   recall session share --id <recall-session-id> --tldr-file /tmp/recall-tldr.md --format json
   ```
   If the TL;DR file is missing, unreadable, or blank, Recall skips the TL;DR
   block and still publishes the session.

4. Reply in this shape:
   ```text
   <url>

   <one-line context: title, source, and whether this was a fresh share or refreshed publish>
   ```
   Do not dump raw JSON unless debugging.

### Failure handling

- If sharing fails because Pages is not configured, tell the user to run `recall share init` once, then retry publish.
- If publish succeeds but the URL still 404s, rerun step 3 without `--dry-run` and report that redeploy finished.

Sharing publishes session content to the configured share target. Warn briefly if the session may contain secrets.

## Scoping Protocol

When the request is broad, such as "use Recall to review this project", do not jump straight to full export. Ask one concise scoping question that makes Recall's value clear:

```text
Default Recall review: historical risk audit, decision archaeology, failed approaches, unfinished work, and current-code assumptions to verify. Should I start with that quick scan, go deep across all project history, or focus on a topic like architecture, tests, performance, a feature, or PR readiness?
```

Do not ask when the user already provides enough scope. Proceed directly when the request includes the project path plus a clear topic, time range, or depth.

Prefer these defaults for broad reviews:

- Analysis mode: historical risk audit + decision archaeology + failed approaches.
- Time range: recent history first, then all history when the user asks for deep analysis or the quick scan finds dense evidence.
- Source scope: all sources unless the user names a source.
- Output: concise findings with historical evidence and current verification steps.

## Analysis Modes

Choose one or more modes based on the user's request:

- Historical risk audit: find repeated problems, long-lived bugs, regressions, unresolved risks, and repeated rework.
- Decision archaeology: recover why designs changed, what trade-offs were made, and which alternatives were rejected.
- Failed approaches: identify attempted fixes or designs that failed, were reverted, or were rejected by the user.
- Current task context: search around a specific feature, file, bug, PR, module, command, or product area before changing code.
- User preference extraction: capture repeated user constraints, style preferences, review standards, forbidden abstractions, and acceptance criteria.
- PR/readiness scan: check whether a proposed change conflicts with historical constraints or known failure modes.
- Project timeline: summarize how the project evolved across sessions.
- Usage/cost analysis: use only when the user explicitly asks about tokens, model usage, or cost patterns.

## Depth Levels

Use a depth that matches the request and data size:

- Quick scan: `recall info`, optional `recall sync`, then targeted `recall search` queries. Use this to find leads quickly.
- Standard review: run several targeted searches, then export a bounded sample such as `--limit 100` or `--limit 300`.
- Deep review: export all matching project sessions with `--limit 0`, parse JSONL structurally, and synthesize themes across sessions.

If the user asks for "deep", "full", "comprehensive", or "analyze history", use deep review unless the export is impractically large. If the export is large, start with a bounded export and report the next deepening step.

## Workflow

1. Identify the project scope and analysis mode.
   - Apply Project Scope Defaults before choosing `--project`; repo identity may span multiple worktree paths.
   - `--project` matches that directory and child paths; it does not match sibling prefixes.
   - Ask at most one scoping question for broad requests.
   - State the default analysis mode when proceeding without a question.

2. Refresh or inspect the index.
   - Run `recall info` to inspect indexed sources and status.
   - Run `recall sync` before relying on recent history, unless the user requested read-only inspection.
   - Use `recall sync --source <source>` only when a single source is relevant.
   - Do not run `recall sync --force` unless the user asks to rebuild or there is evidence the index is stale in a way incremental sync cannot fix.

3. Search before exporting when the question has a target.
   - Use `recall search "<query>" --project /absolute/project/path`.
   - Add `--source <source>` or `--time <range>` to narrow noisy results.
   - Treat search snippets as leads, not complete evidence.
   - Search for both the user's topic and generic project-memory signals such as "failed", "reverted", "decision", "root cause", "regression", "do not", "not again", "unfinished", and "follow-up".

4. Export for deep analysis.
   - Use `recall export --project /absolute/project/path --limit 0` for full project history.
   - Use `recall session export --id <session-id> --format jsonl` for selected sessions.
   - Use `recall session show --id <session-id> --format json --include metadata,messages,usage,events` when one session needs structured inspection.
   - Start with a smaller `--limit` when exploring very large histories.
   - Write temporary analysis artifacts outside the repo unless the user asked for a tracked artifact.
   - Parse JSONL with structured JSON tooling, line by line. Do not parse JSON with grep or ad hoc string splitting.

5. Cross-check conclusions.
   - Use Recall to find prior decisions, failed attempts, constraints, and user preferences.
   - Verify any code behavior, path, command, or invariant against the current repository before changing files.

## Output Protocol

Do not merely report that Recall found sessions. Convert history into useful project memory:

- Historical facts: what prior sessions actually show.
- Evidence: cite source, title or session id, and approximate time when available.
- Repeated patterns: problems or requests that recur across sessions.
- Failed or rejected paths: approaches the agent should not repeat without new evidence.
- Current verification list: assumptions from history that must be checked against current code.
- Actionable next steps: the smallest current-code checks or changes suggested by the history.

For broad project reviews, prefer this shape:

```text
Recall review of <project>:

1. Historical facts that matter now
2. Repeated risks or unresolved problems
3. Failed/rejected approaches to avoid
4. User/project preferences extracted from history
5. Current code assumptions to verify
6. Recommended next verification steps
```

Keep transcript content summarized. Quote only short excerpts when they are necessary evidence.

## Commands For Tool Calls

Use these Recall commands in agent tool calls:

```bash
recall info
recall sync
recall sync --source codex
recall search "migration bug" --project /absolute/project/path
recall search "migration bug" --project /absolute/project/path --source codex --time 30d
recall export --project /absolute/project/path --limit 0
recall export --project /absolute/project/path --source codex --time 30d --limit 100
recall session list --project /absolute/project/path --limit 20 --format json
recall session show --id <session-id> --format json --include metadata,messages,usage,events
recall session export --id <session-id> --format jsonl
recall session share --id <session-id> --format json
recall session share --id <session-id> --tldr-file /tmp/recall-tldr.md --format json
recall session share --id <session-id> --dry-run --format json  # preview only; never use for share/update requests
recall session resume --id <session-id> --print-command
recall import recall-export.jsonl --dry-run
recall import recall-export.jsonl
recall usage --json
```

Supported time filters are `today`, `7d` or `week`, and `30d` or `month`. Unknown time values fall back to all history.

Supported source ids include `claude-code`, `opencode`, `codex`, `pi`, `antigravity-cli`, `gemini-cli`, `grok`, `kiro-cli`, `copilot-cli`, `cursor`, and `cline`. Source labels such as `CC`, `OC`, `CDX`, and `CUR` are also accepted by the CLI, but source ids are clearer in scripts.

## Export Schema

`recall export` and `recall session export --format jsonl` emit one JSON object per indexed session. `recall session show --format json` uses the same top-level record schema for one selected session. Each record includes:

- `schema_version`
- `record_type`
- `session`
- `messages`
- `usage_events`
- `events`

Since schema_version 3 the export is lossless against the Recall index: it also carries `session.source_file_path`, usage-event `parser_version` / `source_path` / `raw_usage_json`, and event `attrs_json` / `parser_version`. Expect optional fields to be `null`. `recall import` accepts schema_version 2 and 3 files and skips sessions that already exist locally by `(source, source_id)`.

Important fields:

- `session.source`: the adapter id, such as `codex` or `claude-code`.
- `session.source_id`: the original tool's session id.
- `session.title`: the indexed session title.
- `session.directory`: the project directory captured for the session.
- `session.started_at` and `session.updated_at`: millisecond timestamps.
- `messages[].role`: `user` or `assistant`.
- `messages[].content`: the indexed message text.
- `usage_events[]`: token usage when available.
- `events[]`: tool/session events when available, including `attrs_json` raw attributes.

## Avoid In Agent Tool Calls

Do not use these as the primary workflow for analysis:

- `recall` with no subcommand: launches the TUI.
- `recall usage` without `--json`: launches a dashboard flow.
- `recall session share --dry-run` when the user asked to share, update, or refresh a link.
- Returning a URL without running `recall session share` (no `--dry-run`).
- Resume or app-launch behavior from the TUI: it opens another interactive session and has side effects.
- Hidden commands such as `__bench-*` or `__background-worker`: internal or development-only.
- Raw source transcript paths: use the public export unless the user explicitly asks for source-level forensics.

## Privacy And Output

Session history can contain private code, prompts, credentials, and user intent. Summarize only what is needed for the task. Do not paste full transcripts unless the user explicitly asks for them.
