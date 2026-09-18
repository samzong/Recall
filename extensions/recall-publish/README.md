# recall-publish

Prepares a redacted, reviewable dataset from local Recall sessions and uploads
it to a Hugging Face dataset repository. Installed as `recall publish`.

Publishing is a four-step gate. Nothing leaves the machine until the publisher
inspects the prepared workspace and approves the exact bytes.

```
recall publish doctor --install
recall publish prepare --project Recall --since 2026-08-01 --until 2026-09-01 \
    --license CC-BY-4.0 --author samzong
recall publish approve author-samzong_project-recall_time-2026-08
recall publish upload author-samzong_project-recall_time-2026-08 --repo samzong/recall-sessions
```

## Commands

| Command | Effect |
| --- | --- |
| `doctor` | Reports Recall, gitleaks, uv, and `hf` availability. `--install` materializes and syncs the redaction environment. |
| `prepare` | Selects sessions, removes local paths, redacts secrets and identities, writes the workspace, rescans, and prints a preview. |
| `approve` | Binds the current workspace digests. Any later change invalidates the approval. |
| `upload` | Uploads the approved data file and manifest, then reads them back and verifies the digests. `--dry-run` shows what would be sent. |

## Progress

`prepare` reports each phase on stderr and keeps stdout to JSON alone, so it
stays pipeable. In a terminal the redaction phase shows a running field counter;
it is the slow phase, and the first batch also loads the language models. Pass
`--quiet` to silence stderr entirely.

## Selection

`--project`, `--source`, `--thread-role`, `--since`, `--until`, `--timezone`,
`--session`, and `--exclude-session`. The interval is half-open on the session
start timestamp. A `--session` id is published even when it falls outside the
interval; `--exclude-session` wins over every other selector.

## Redaction

Local filesystem locations are removed rather than redacted. Remaining text is
scanned by gitleaks for credentials and by Presidio with English and Chinese
spaCy models for identities, plus mainland phone number and identity card
recognizers. A finding that cannot be mapped back to its field blocks
publication, and the prepared file is rescanned before the manifest is written.

Redaction is a floor, not a guarantee. Internal decisions, customer context, and
unusual identifiers can survive it, so the workspace is meant to be read before
`approve`.

## Configuration

`<config_dir>/recall/publish.json` sets `publisher.author`, `languages`,
`allow.identities`, `allow.gitleaks_rules`, `allow.presidio_entities`, and
`path_substitutions`. Every weakened detector is recorded in the published
manifest.

## Requirements

`gitleaks`, `uv`, and the `hf` CLI on PATH, plus Recall 0.6.0 or newer. The
redaction environment lives under the Recall data directory and is created by
`doctor --install`.

## Named entities

`PERSON` and `LOCATION` ship in the default `allow.presidio_entities`, so named
entities are not redacted. spaCy assigns every named-entity span the same fixed
score, so product and platform names such as `Claude`, `DeepSeek`, and `Linux`
are indistinguishable from real names. A measurement over two real sessions
produced 3391 `PERSON` and 222 `LOCATION` markers against 19 `IP_ADDRESS` and 4
`EMAIL_ADDRESS`, which leaves the transcripts unreadable.

A contributor name written in prose can therefore reach the published dataset.
Set `allow.presidio_entities` to an empty list to redact named entities and
accept the noise. Gitleaks secrets and every pattern-backed entity are redacted
either way, and the published manifest records which entity types were not.
