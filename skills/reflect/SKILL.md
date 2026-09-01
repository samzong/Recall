---
name: reflect
description: Use Recall Reflect to review AI coding workflow history as a conversation-first timeline and discuss observed patterns before changing behavior. Trigger when the user asks to reflect on a project, review AI coding workflow, inspect workflow friction, discuss repeated corrections, review handoffs, or explore possible calibration.
---

# Reflect

## Overview

Reflect is the official `recall-reflect` Recall extension. It is a discussion-first workflow for reviewing AI coding sessions through Recall's stable CLI JSON and JSONL protocol. The extension reconstructs a conversation-first timeline across sessions and surfaces observed patterns as questions for the user, not as automatic conclusions or edits.

Use this skill when the user wants project-level reflection on AI coding process, workflow friction, handoffs, repeated corrections, scope drift, or possible future calibration.

Reflect output is evidence for discussion, not permission to change skills, prompts, configs, instruction files, or project files.

## Core Rule

Do not treat an observed pattern as a calibration target, workflow change, or proposal unless the user explicitly confirms the interpretation. Ask first, then act only on confirmed direction.

## Scope Defaults

Scope to the current repo unless the user asks otherwise. Resolve the project directory with:

```bash
git rev-parse --show-toplevel
```

Run Reflect with structured output. Prefer user-facing dispatch when the extension is installed and managed by Recall:

```bash
recall reflect --project /absolute/project/path --format json
```

Direct extension invocation is also acceptable when testing or calling the extension binary explicitly:

```bash
recall-reflect --project /absolute/project/path --format json
```

Add `--time 30d` or `--source <source>` when the user narrows the review. Use `--sync` when recent sessions may not be indexed yet.

## Workflow

1. Resolve the repo scope.
   - Use the repository root for current-repo reflection.
   - Use the user-provided path when they specify one.
   - Explain the scope briefly if there may be multiple worktrees or project directories.

2. Run Reflect.
   - Prefer `recall reflect ...` when Recall can dispatch to the installed extension.
   - Use `recall-reflect ...` for direct testing or explicit extension invocation.
   - Prefer JSON for structured inspection.
   - Use text output only when the user wants a human-readable report directly.

3. Read the timeline as conversation history.
   - Focus on user intent, agent response, corrections, decisions, and outcomes.
   - Do not turn the timeline into a tool-call log.
   - Do not quote large transcript blocks.

4. Present observed patterns as discussion prompts.
   - Use the prompt text from `observed_patterns[].discussion_prompt` when present.
   - If there are no observed patterns, summarize the timeline and say there is not enough repeated evidence yet to discuss a pattern.
   - Do not add confidence scores, personality labels, or identity judgments.

5. Discuss before proposing changes.
   - Ask which pattern, if any, the user wants to inspect further.
   - Ask whether the pattern is real, intentional, or a false positive.
   - Only after user confirmation may you suggest a calibration target, workflow rule, or follow-up implementation plan.

## Output Shape

Prefer concise discussion framing:

```text
Reflect review of <project>:

1. Timeline in plain language
2. Observed pattern prompts
3. One question for the user
```

When a pattern appears, ask about it directly:

```text
The timeline shows repeated scope-boundary corrections. Is this a real workflow issue worth calibrating, or are these unrelated reminders?
```

## Commands

```bash
recall reflect --project /absolute/project/path --format json
recall reflect --project /absolute/project/path --time 30d --format json
recall reflect --project /absolute/project/path --source codex --format json
recall reflect --project /absolute/project/path --sync --format json
recall reflect --project /absolute/project/path --format text
recall-reflect --project /absolute/project/path --format json
```

Supported time filters are `today`, `7d` or `week`, and `30d` or `month`. Unknown time values fall back to all history.

Supported source ids include `claude-code`, `opencode`, `codex`, `pi`, `antigravity-cli`, `gemini-cli`, `grok`, `kiro-cli`, `copilot-cli`, `copilot-chat`, `cursor`, and `cline`.

## Guardrails

- Do not present Reflect output as a diagnosis of the user.
- Do not infer private intent beyond what the timeline shows.
- Do not turn a pattern into a rule without explicit user confirmation.
- Do not edit files from Reflect output alone.
- Verify any technical claim against the current repository before acting on it.
- Keep transcript content summarized. Quote only short excerpts when necessary.
