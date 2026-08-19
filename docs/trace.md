# Trace CLI PRD

Status: design proposal. Not implemented. Feasibility measured against a local
index of 6,953 sessions / 400,560 messages / 13 sources.

## Goal

Given a commit, a file, or a line, return the session that produced it, and the
reasoning that never reached the repository: what was evaluated, what was
rejected, and what was known but left undone.

Search answers "which session mentions X" and requires the user to already know
the keyword. `recall trace` answers "why does this code exist" from a coordinate
the user already holds.

## Problem

Git records *what* changed. It has never recorded *why*. A commit message is a
lossy summary written after the fact, and the part it drops is the reasoning.

Worked example from this repository, 2026-08:

```text
4aa5aee  chore(release): move core releases to release-please (#123)
42172b0  fix(release): scope release-please to the root package (#125)
dad8eba  revert(release): put version choice back in the maintainer's hands (#128)
```

Twenty-seven words, and the code was reverted, so the tree retains nothing. The
session covering that window holds a seven-tool comparison (git-cliff,
release-please, semantic-release, changesets, cocogitto, release-plz,
standard-version), a selection of git-cliff + cargo-release with three stated
reasons, and the fact that what actually shipped was a different tool than the
one the analysis chose. None of that is reachable from the code.

The same session contains a plan that measurement killed: a baseline showed the
adapters targeted by a proposed pre-parse filter cost 130 ms combined, while
4,040 of 4,258 candidates were already rejected by mtime. The commit that
followed (`0570dde`) describes the conclusion accurately and says nothing about
the plan it replaced. Nothing prevents that plan from being proposed again.

## Evidence

### Alignment is high enough to build on

Share of commits with same-repo session activity in the preceding window:

| Repository        | Commits | Sessions | 2h  | 6h  | 24h |
| ----------------- | ------: | -------: | --: | --: | --: |
| `samzong/Recall`  |     163 |      386 | 71% | 85% | 98% |
| `lathe-cli/lathe` |     159 |      450 | 78% | 86% | 97% |

### A blind sample recovers reasoning in 70% of commits

Ten commits drawn with a fixed seed from the 120 most recent non-release commits
in this repository, spanning fix / revert / docs / perf / feat / refactor /
build. Acceptance required recovering reasoning **absent from the commit
message**; locating a plausible session did not count.

```text
strict pass (reasoning not in the commit message)   7/10   70%
located the correct session                         9/10   90%
excluding one commit authored by an outside PR      7/9    78%
```

Two of the three non-passes were commits whose messages were already 193 and 63
words and already carried the rationale. The other eight messages in the sample
were single lines of 5-13 words. **The value of this feature is inversely
correlated with commit message quality, and the distribution favors it.**

The single miss was authored by an outside contributor, so no local session
exists. A commit authored by a different contributor but *reviewed* locally was
recovered normally: participation, not authorship, is what leaves a trace.

## Definitions

- **Coordinate** — a commit-ish, a path, or a `path:line` the user already has.
- **Candidate** — a session in the same repository whose activity precedes the
  coordinate's commit within the time window.
- **Attribution** — a scored candidate plus the evidence that produced the score.
- **Recovered reasoning** — evaluated options, rejected options and their
  reasons, and known-but-unaddressed items, extracted from an attributed session.

## Scope

### In Scope

- Resolve a coordinate to ranked candidate sessions with explicit confidence.
- Show the evidence behind each candidate: matched paths, time delta, source.
- Extract recovered reasoning from an attributed session, computed offline and
  persisted.
- `--format json` for agent consumption, matching the existing CLI contract.

### Out of Scope

- Claiming causation. Alignment is correlation; the command reports confidence
  and never asserts that a session produced a commit.
- Backfilling sessions whose source transcripts were deleted. Titles and derived
  fields are computed at sync time; a missing transcript cannot be re-read.
- Attributing commits authored by contributors whose sessions are not on this
  machine.
- Writing to Git. This command reads; it does not add trailers or notes.
- Repository-wide sweeps such as "which commits have no recoverable session".
  See Storage.
- Any dependency on the SQLite schema or Rust internals from outside core.

## Command Design

### `recall trace <commit-ish>`

```text
$ recall trace dad8eba

commit  dad8eba  revert(release): put version choice back in the maintainer's hands
session claude-code/189694a9  ·  1,249 messages  ·  confidence high
        matched 12 changed paths, 437 mentions, started 10h07m before the commit

evaluated   git-cliff / release-please / semantic-release / changesets
            cocogitto / release-plz / standard-version
selected    git-cliff + cargo-release
            reason: repo already uses conventional commits; CHANGELOG can enter
            the release commit and therefore the tag itself
rejected    release-please
open        final release mechanism left to the maintainer
```

### `recall trace <path>[:<line>]`

```text
$ recall trace src/handoff.rs:23

introduced  faeecb1  feat(session): add agent handoff flow (#58)
session     codex/018f6650  ·  confidence medium

decided     handoff targets one tool at a time (1-to-1)
known-open  transcripts above ARG_MAX fail; not handled
            (measured: 2% of sessions exceed 1 MB, largest 4.2 MB)
```

#### Line coordinates resolve through `git log -L`, not single-line blame

Single-line blame answers "who last touched this line", which is frequently the
wrong commit.

Measured: `src/utils.rs:179` is the signature of `is_noise_first_message`, and
blame attributes it to `6163c7df` (2026-04-13). The commit that actually changed
that function's behavior is `3545698`, which rewrote the body and the doc
comment and left the signature untouched. A user asking why the function looks
the way it does would receive a session from four months before the change they
are looking at.

Take the enclosing range rather than the single line, walk its history with
`git log -L <start>,<end>:<file>`, and attribute every commit in the chain. Show
the most recent by default; `--all` shows the full evolution.

### Flags

| Flag             | Default | Notes                                       |
| ---------------- | ------- | ------------------------------------------- |
| `--window <dur>` | `24h`   | Lower values lose recall; see Matching.     |
| `--limit <n>`    | `3`     | Candidates to show.                         |
| `--all`          | off     | Line coordinates: show the full change chain. |
| `--format json`  | text    | Stable agent contract.                       |
| `--reasoning`    | off     | Run extraction. Off by default: attribution is deterministic and fast, extraction is neither. |

### JSON shape

```json
{
  "coordinate": { "kind": "commit", "value": "dad8eba" },
  "commit": { "sha": "dad8eba", "subject": "...", "committed_at": 1786465844 },
  "candidates": [
    {
      "source": "claude-code",
      "source_id": "189694a9-6ba2-4d00-924c-c0f31e68e3f7",
      "confidence": "high",
      "evidence": {
        "matched_paths": ["src/utils.rs"],
        "mention_count": 437,
        "lead_time_seconds": 36420
      },
      "reasoning": {
        "evaluated": ["git-cliff", "release-please"],
        "selected": [{ "what": "git-cliff + cargo-release", "because": "..." }],
        "rejected": [{ "what": "release-please", "because": "..." }],
        "open": ["final release mechanism left to the maintainer"]
      }
    }
  ]
}
```

## Matching

Four signals, applied in order. Each was validated during the sample run above.

1. **Repository identity.** Filter by `repo_slug` / `repo_remote`. Sessions with
   an empty `repo_name` are correctly excluded: measurement showed 0 of 3,918
   empty values came from a defect — 88% are directories that no longer exist,
   the rest are home directories, non-git directories, or repos without an
   origin. Those sessions were never in a repository and are out of scope.
2. **Time window, 24h.** Not 6h. Several commits in the sample had zero
   candidates at 6h and the correct one at 24h.
3. **Changed-path mentions, excluding tool output.** Rank candidates by how many
   of the commit's changed paths appear in the session. **Tool output must be
   filtered before scoring.** In the first sample run the top-ranked messages for
   one commit were entirely `git status` and `gh auth` output: changed paths
   occur most densely in tool results, and tool results contain no reasoning.
4. **Confidence, reported not hidden.** Alignment is correlation. Report the
   band and the evidence that produced it rather than asserting a single answer.

## Data Model

The relation is many-to-many in both directions, and this is not a corner case:

- One session covering 2026-08-11 corresponds to at least 8 commits that day.
- `6714f61` has its execution in one session (a ship workflow) and its rationale
  in a different one (a code review report that named `sync.rs`'s 433-line
  function as a violation hotspot).

Taking only the nearest session in time loses the second case. Attribution is
computed as `(commit, session)` edges carrying per-edge evidence, never as a
single session pointer on a commit.

## Storage

**No new schema. Attribution is recomputed per query.**

Matching is cheap — a time-window filter over sessions already indexed by
`repo_slug`, plus a substring count across a handful of candidates. For one
coordinate that is milliseconds, it adds no migration risk, and it leaves the
matching algorithm free to change without invalidating a persisted cache.

Extraction is the expensive half, and it is the only thing persisted: keyed by
`(source, source_id)` plus a content fingerprint, computed offline, never per
query.

The accepted cost is that repository-wide sweeps are slow. Those are out of
scope; revisit this decision only if such a query becomes a real request.

## Session Identity

**Use `source` + `source_id`. Never persist `sessions.id`.**

`sync` regenerates `Uuid::new_v4()` on every `RefreshSession`, and the trigger is
`content_changed || force` — so any session that is still being worked in gets a
new id on the next sync. A plain incremental sync was observed changing ids for
sessions whose message count had not changed at all.

This is already the established pattern: `share_id_for_session` in
`src/share/render.rs` prefers `source_id`, and resume/open dispatch on
`source_id`. Trace follows the same rule, so no data-model change is required.

Should commit trailers ever be emitted (out of scope here), the same identifier
applies:

```text
Recall-Session: codex/019ef07d-d411-7e21-8...
```

## Extraction

Recovering "evaluated / selected / rejected / open" from a 1,249-message session
is a summarization task with real cost and a real error rate. Constraints:

- Off by default, behind `--reasoning`. Attribution is deterministic and returns
  in milliseconds; extraction is neither, and the default path should not
  inherit its cost or its error rate.
- Compute offline and persist, as described in Storage.
- Treat output as evidence, not truth, consistent with how `skills/recall`
  already frames session history.
- Degrade honestly: when extraction is unavailable, attribution alone is still
  useful and remains the fallback.
- Cover review verdicts, not just implementation decisions. In the measured
  index 31.4% of sessions open with a review, audit, or evaluation request, so a
  large share of recoverable reasoning is "this was reviewed and rejected"
  rather than "this was designed this way".

## Privacy

**Personal-only. No redaction.**

Sessions contain pasted logs, tokens, and internal discussion. This command does
not make that worse: `recall session show` already prints full transcripts, and
`recall trace` returns a summary of the same data, on the same machine, to the
same user. Redaction here would protect nothing while creating a false sense of
safety — a user who believes the output is scrubbed is more likely to paste it
somewhere it does not belong.

If a team-facing surface is ever built, its permission boundary is designed then,
against a real sharing model, rather than guessed at now.

## Verification Plan

The feature is falsifiable, and the criterion is fixed before implementation.

1. Ship attribution only — coordinate in, ranked candidates out, no extraction.
2. Use it for two weeks and count deliberate lookups.

| Outcome                                                            | Action                            |
| ------------------------------------------------------------------ | --------------------------------- |
| >= 5 lookups, and at least one avoided redoing a rejected approach  | Build extraction, then team scope |
| 1-4 lookups                                                         | Hold two more weeks; add nothing  |
| 0 lookups                                                           | Stop; the premise was wrong       |
