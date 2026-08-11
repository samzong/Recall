# KEP: recall-reflect

## Metadata

- **Status**: Provisional
- **Type**: Feature
- **Created**: 2026-07-09
- **Owners**: Recall maintainers
- **Extension**: `recall-reflect`
- **Stage**: phased design
- **Related design**: `docs/extensions.md`

## Summary

`recall-reflect` is the official Reflect extension for Recall. It turns local,
multi-agent AI coding history into an evidence-backed review workflow for how a
person works with coding agents.

Reflect supports two first-class views:

- **Personal reflection**: cross-project, cross-agent reflection over a recent
  time window.
- **Project reflection**: repository-scoped reflection over a project timeline.

The current implementation is a text/JSON project-timeline report. The next
milestone adds explicit personal and project scope semantics so Reflect can
support both cross-project personal reflection and repository-scoped continuity.
Later milestones can add an extension-owned TUI review workbench, proposal
previews, and explicit instruction-file patches.

The long-term value loop is:

```text
Reflect
  -> actionable observations
  -> evidence review
  -> proposal preview
  -> user approval
  -> instruction patch
```

## Motivation

Recall already indexes local sessions from multiple coding agents and exposes
stable JSON/JSONL query surfaces. Current workflows are session-first: search,
inspect, export, share, and usage reporting.

AI coding work is not session-first in practice. A user may plan in Claude Code,
implement in Codex, review in OpenCode, restart with a cleaner context, copy one
agent's output into another, or repeatedly correct the same behavior across
projects.

Provider-specific reflection dashboards can only see one product's history.
Recall can reflect over mixed local history from many coding agents and show
handoffs, source roles, repeated corrections, and tool-switching friction that
single-provider tools cannot see.

Reflect should not stop at insight. A report that only says "you often correct
scope drift" has weak user value. The product should eventually help the user
apply a confirmed lesson to future work, starting with small, previewed patches
to repository instruction files such as `AGENTS.md` or `CLAUDE.md`.

### Goals

- Support personal and project reflection as first-class views.
- Default to project reflection inside a Git repository.
- Allow `--personal` to force personal reflection, even inside a repository.
- Default to personal reflection outside a Git repository.
- Preserve the current text/JSON report path while scope semantics mature.
- Add a TUI-first review workbench for observations, evidence, and proposed
  actions in a later milestone.
- Surface repeated friction, source roles, cross-agent handoffs, and workflow
  patterns as observations, not verdicts.
- Require supporting evidence for each actionable observation.
- Apply only explicit, previewed instruction-file patches once the apply
  milestone exists.
- Consume Recall data only through stable CLI JSON/JSONL output.
- Preserve machine-readable output for agents with `--format json`.

### Non-Goals

- Do not make Reflect a personality profile or scorecard.
- Do not rank agents as a universal leaderboard.
- Do not enforce quiet hours, usage limits, or wellbeing nudges in Recall core.
- Do not mutate Recall's SQLite database directly from the extension.
- Do not expose Rust internals or a `recall-core` library crate.
- Do not build a general-purpose patch engine for arbitrary project files.
- Do not auto-edit skills, prompts, project files, or instruction files without
  user approval.
- Do not replace `recall search`, `recall session`, `recall export`, or the core
  Recall TUI.

## Proposal

Add `recall-reflect` as an official extension that consumes Recall through the
stable CLI protocol and owns the reflection workflow.

The extension reconstructs conversation-first timelines for a selected scope and
detects actionable workflow observations. The initial implementation presents
those observations in text and JSON. Later milestones add TUI evidence review,
proposal drafts, and instruction patches that apply only after explicit user
approval.

### User Stories

#### Personal reflection

As a user who mixes Claude Code, Codex, OpenCode, and other agents, I want to
see how I used them over the last week or month, so I can understand which
agents I use for planning, implementation, review, debugging, and cleanup.

#### Project reflection

As a user starting work in a repository, I want `recall reflect` to summarize
recent project sessions and repeated friction, so I can continue work with the
right constraints in mind.

#### Cross-agent handoff review

As a user who moves work between agents, I want Reflect to show where handoffs
worked or created friction, so I can adjust how I split tasks across tools.

#### Apply a lesson

As a user who repeatedly corrected an agent for scope expansion, I want Reflect
to propose a small `AGENTS.md` rule and show the exact diff before applying it,
so future agents receive the constraint earlier.

### Implemented Baseline

The current `recall-reflect` implementation is intentionally smaller than the
full product direction:

- it is an official external extension binary named `recall-reflect`;
- it consumes `recall export --include metadata,messages`;
- it supports `--personal`, `--project`, `--repo`, `--source`, `--time`,
  `--sync`, and `--format text|json`;
- when no project or repo is provided inside a Git repository, it infers the
  current Git root as project scope;
- when no project or repo is provided outside a Git repository, it defaults to
  personal scope over the recent `30d` window;
- it renders project and personal conversation timelines, chunks long sessions,
  filters low-level transcript artifacts, emits deterministic source-role,
  project-activity, and task-shape summaries, and emits a small
  discussion-prompt layer.

The implemented baseline does not yet support a TUI output mode, proposal
persistence, or instruction-file patch application.

### Notes And Constraints

- Core remains the data plane and stable query protocol owner.
- Reflect is an external executable named `recall-reflect`.
- Reflect must not read `recall.db` directly.
- Transcript reflection should work from `metadata,messages`.
- Usage and event records are optional context for timing, token-heavy loops, or
  source-role analysis.
- Progress and diagnostics go to stderr. Machine output stays on stdout.

## Design Details

### Scope Resolution

Reflect supports explicit and inferred scopes:

1. `--personal` forces personal reflection, even inside a repository.
2. `--project <path>` or `--repo <identity>` forces project reflection.
3. With no explicit scope inside a Git repository, Reflect defaults to the
   current repository.
4. With no explicit scope outside a Git repository, Reflect defaults to personal
   reflection over a recent time window.

The selected scope must be explicit in TUI, text, and JSON output. Personal
reflection must not silently include all indexed history unless the user chooses
a broad time window.

The implemented default personal time window is `30d`. Future releases can tune
that default if user testing shows a different recent window is more useful.

### Command Surface

```bash
recall reflect
recall reflect --personal --time 30d
recall reflect --project /path/to/repo
recall reflect --repo owner/repo
recall reflect --personal --source codex --source claude-code --time 7d
recall reflect --format json
```

Options:

- `--personal`: reflect across projects for the selected time/source scope.
- `--project <path>`: project directory boundary, including child paths.
- `--repo <identity>`: repository identity such as `owner/repo` or a remote URL.
  Cannot be combined with `--project`.
- `--time <today|7d|week|30d|month|all>`: time window.
- `--source <source>`: optional source filter. Repeated values mean a
  mixed-source reflection. The current implementation accepts one source; repeated
  source values are a later enhancement.
- `--format <text|json>`: implemented output modes. A future TUI milestone may
  add `tui` and make it the default interactive mode.
- `--sync`: optionally run incremental sync before reflection.
- `--include-events`: planned option to include summarized low-level events as
  supporting context.
- `--include-usage`: planned option to include usage records when available.

### Future TUI Review Workbench

The default TUI should prioritize action over dashboard polish:

```text
[ Observations ]        [ Evidence ]              [ Proposal / Diff ]
scope drift x4          session + moment ids       AGENTS.md patch
missing tests x3        source + timestamp         Apply / Dismiss
handoff friction x2     concise excerpts           Save note / Draft
```

The left pane is an actionable observation queue. The middle pane explains why
an observation exists. The right pane shows what the user can do with it.

Supported MVP actions:

- **Dismiss**: reject the observation.
- **Save note**: keep the observation without modifying a repository.
- **Draft**: create a proposal but do not apply it.
- **Apply**: apply a previewed instruction-file patch after confirmation.

When personal reflection runs outside a repository, actions that require an
instruction patch should ask the user to choose a target repository or stay in
note/draft mode.

### Observation Model

Reflect should identify repeated or notable workflow patterns, including:

- repeated scope expansion or scope-boundary reminders;
- repeated missing verification or testing reminders;
- repeated over-engineering or simplification requests;
- cross-session continuation of the same task;
- cross-agent handoffs where one source shapes the next prompt or direction;
- recurring source roles, such as exploration in one agent and implementation in
  another;
- repeated planning, debugging, review, or verification loops;
- disagreement between user and agent about scope or timing.

Each actionable observation should include:

- stable observation id;
- concise summary;
- source roles or affected sources when relevant;
- supporting session ids and moment ids;
- representative excerpts or summaries;
- discussion prompt;
- optional proposal draft.

### Future Proposal And Apply Model

The first apply target should be a repository instruction file:

- `AGENTS.md`;
- `CLAUDE.md`;
- future instruction files discovered by project conventions.

Proposal previews must show:

- target file;
- exact diff;
- evidence moments that justify the rule;
- what the user must approve before anything is written.

Apply must be separate and deliberate. It must never happen from observation
detection alone.

### Text Output

Text output should be concise and layered:

```text
Recall reflect for <scope>:

1. Scope summary
2. Timeline
3. Source roles
4. Project activity
5. Task shapes
6. Observed workflow patterns
7. Discussion prompts
8. Calibration targets confirmed by the user
9. Optional proposals
10. Follow-up checks from previous calibrations
```

The main timeline should read like a concise history of human intent and agent
response, not a tool log.

### JSON Output

JSON output should preserve enough structure for agents to continue the
conversation:

```json
{
  "scope": {
    "kind": "project",
    "project": "/path/to/repo",
    "repo": "owner/repo",
    "time_range": "30d",
    "sources": ["codex", "claude-code"]
  },
  "source_roles": [
    {
      "source": "claude-code",
      "observed_role": "Exploration and broad planning",
      "sessions": 2,
      "timeline_moments": 6,
      "evidence_moments": ["moment-1", "moment-3"]
    }
  ],
  "project_summaries": [
    {
      "project": "/path/to/repo",
      "sessions": 3,
      "timeline_moments": 12,
      "sources": ["codex", "claude-code"]
    }
  ],
  "task_shapes": [
    {
      "shape": "planning",
      "timeline_moments": 4,
      "evidence_moments": ["moment-1", "moment-5"]
    }
  ],
  "timeline": [
    {
      "id": "moment-1",
      "timestamp": 1781234567890,
      "source": "codex",
      "session_id": "...",
      "kind": "conversation",
      "summary": "User narrowed scope; agent proposed a broader implementation."
    }
  ],
  "observed_patterns": [
    {
      "id": "pattern-1",
      "summary": "Scope expansion appeared after narrow requests.",
      "timeline_moments": ["moment-1", "moment-4"],
      "discussion_prompt": "Is this a workflow issue worth calibrating?"
    }
  ],
  "proposals": []
}
```

## Risks And Mitigations

### False pattern detection

Reflect may infer a pattern from unrelated moments.

Mitigation: phrase findings as observations and prompts, require evidence ids,
and require user confirmation before proposals or patches.

### Over-broad personal reflection

Personal reflection can combine private intent across projects.

Mitigation: default to a recent window, make the scope explicit, summarize by
default, and avoid broad transcript excerpts unless requested.

### Unsafe apply behavior

Instruction patches can change future agent behavior.

Mitigation: MVP apply only targets instruction files, always shows a diff, and
requires explicit user approval.

### Core boundary creep

Reflection features could pressure Recall core to become a workflow engine.

Mitigation: keep Reflect as an extension consuming stable CLI output. Core stays
the data plane and query protocol.

## Graduation Criteria

### Implemented Baseline

- `recall-reflect` is an official extension binary with manifest support.
- Project and personal reflection work from `metadata,messages`.
- Text and JSON output render the selected scope kind, project/repo scope,
  timeline phases, source-role summaries, project-activity summaries, task-shape
  summaries, observed patterns, and proposal stubs.
- Low-level transcript artifacts are hidden or summarized by default.

### Completed Milestone: Personal And Project Scopes

- Scope resolution follows the explicit/personal/project rules above.
- `--personal` forces personal reflection, including inside a Git repository.
- `recall reflect` inside a Git repository defaults to project reflection.
- `recall reflect` outside a Git repository defaults to personal reflection over
  the recent `30d` time window.
- Text and JSON output include a stable scope kind.

### Completed Milestone: Broader Reflection Signals

- Personal reflection provides deterministic source, project, and task-shape
  summaries without reading SQLite directly.

### Next Milestone: Interactive Review

- `recall reflect` can open a TUI review workbench.
- TUI shows actionable observations with evidence ids.
- TUI previews instruction-file patches.
- Apply writes only approved instruction-file patches.

### Later Milestone: Interactive Review Polish

- `recall reflect` opens the TUI by default.
- `--format json` emits machine-readable scope, observations, evidence, and
  proposal stubs.

### Later Milestone: Persistence And Follow-Up

- Saved notes and draft proposals have an extension-owned persistence model.
- Cross-agent handoff detection is robust enough for mixed-source histories.
- Usage/events can enrich selected personal reflection layers without becoming
  the main report.
- Follow-up reflection can compare sessions before and after accepted patches.

### Stable

- Proposal targets beyond instruction files are intentionally selected and
  covered by tests.
- Personal and project reflection have stable output semantics.
- TUI flows are covered by regression tests for scope selection, preview, apply,
  and cancel paths.

## Production Readiness Review

### Operational impact

Reflect runs as an external command. It should not add background services or
long-running core processes in the MVP.

### Security and privacy

Reflect reads local session history through Recall's CLI. It must not upload
data or share reports by default. Sensitive content should be summarized at a
high level in personal reflection.

### Observability

Machine output belongs on stdout. Progress, warnings, sync messages, and apply
diagnostics belong on stderr.

### Rollback

Instruction patches should be normal file diffs in the user's repository. Users
can review, revert, or edit them with standard Git workflows.

## Alternatives

### Report-only Reflect

A report-only workflow is simpler, but it produces weaker user value. It helps
users see patterns without helping them change future agent behavior.

### Core TUI integration first

Embedding Reflect into Recall's core TUI would make it feel native, but it would
increase core surface area and test complexity. An extension-owned TUI keeps the
boundary cleaner.

### Usage dashboard extension

A usage-first dashboard would overlap with existing usage reporting. Reflect
should use usage as optional supporting context, not as the main product.

### Agent leaderboard

Ranking agents is easy to understand but misleading. Different sources are used
for different tasks. Reflect should describe source roles and friction instead
of declaring a universal winner.

## Open Questions

- What should the default personal reflection window be outside a repository:
  7 days, 30 days, or another recent range?
- Should saved notes and draft proposals be written as extension-owned files, or
  only emitted in reports at first?
- After instruction-file patches, which proposal targets should come next:
  skills, project workflow documents, or personal agent preferences?
- How should Reflect show optional low-level event context without turning the
  main report into a tool log?
- Which personal reflection layers should use usage/events data, and which
  should stay transcript-only?
- How should cross-agent handoffs be detected without relying on fragile source
  ordering or full transcript prompts?

## Implementation History

- 2026-07-09: Initial Reflect design reframed as a KEP-style proposal with
  personal and project views, TUI-first MVP, and instruction patch apply loop.
