# crates/rx/

`rx` launches installed native harnesses. It is not a harness distribution
or a Recall core extension. It owns provider selection, its credential store,
generated catalogs, and the minimum adapter state needed to launch. It does
not depend on Recall core or read `recall.db`. Provider admission: `PROVIDERS.md`.

## Principles

- Native first: execute the installed harness and preserve its behavior.
- User ownership: users own installation, sessions, and unowned config.
- Scoped adaptation: inject at launch; persist only when the harness requires it.
- Explicit ownership: location or matching content never proves rx ownership.
- Harness fidelity: do not force different CLIs through a behavior-changing
  common abstraction.
- No inference: missing product decisions require the owner; do not invent
  defaults or boundaries.

Owner decisions outrank this file and must update it in the same change.
Conflicting code is a bug.

## Rules

- Use the official installer; do not relocate the native CLI.
- Passthrough when provider is `none` or unconfigured. An OpenRouter key still
  selects OpenRouter when nothing else is set.
- Keep argv as OS strings; everything after `--` is literal.
- Persistent native edits need explicit ownership, unowned-data preservation,
  locking, atomic writes, and fail-closed parsing.
- No credentials in argv, logs, or world-readable files.
- Hosted state is selected-harness-only, after install. Do not override
  unrelated harness homes (`DSH_HOME` stays user-owned).
- New harness: owner decision, then CLI, alias, install, route, ownership,
  secrets, hosted isolation, docs, `cargo test -p rx`, and `make check`.

Owned: `~/.recall/{rx.toml,rx.keys,catalogs/}`. Shared via markers: Claude
catalog, Pi `models.json`, Kimi `config.toml`. Launch-only: Codex, OpenCode.
DSH home is user-owned; routing is a launch overlay.
